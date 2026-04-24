"""Minimal tests for the MA crossover template.

Run with: pip install -e .[test] && pytest -q
"""

from __future__ import annotations

from collections import deque
from decimal import Decimal

import httpx
import pytest
import respx

from tektii import AsyncTradingGateway, CandleEvent

from strategy import (
    Config,
    Cross,
    MaCrossoverStrategy,
    StrategyState,
    bracket_prices,
    compute_sma,
)


def _bar(close: str, *, timestamp: str = "2026-04-20T10:00:00Z") -> dict:
    return {
        "symbol": "EUR/USD",
        "provider": "mock",
        "timeframe": "1m",
        "timestamp": timestamp,
        "open": close,
        "high": close,
        "low": close,
        "close": close,
        "volume": "1000",
    }


def _candle(close: str, symbol: str = "EUR/USD") -> CandleEvent:
    return CandleEvent.model_validate(
        {
            "type": "candle",
            "timestamp": "2026-04-20T10:00:00Z",
            "bar": {
                "symbol": symbol,
                "provider": "mock",
                "timeframe": "1m",
                "timestamp": "2026-04-20T10:00:00Z",
                "open": close,
                "high": close,
                "low": close,
                "close": close,
                "volume": "1000",
            },
        }
    )


def test_compute_sma_matches_manual_average() -> None:
    values = deque([Decimal("10"), Decimal("20"), Decimal("30")], maxlen=3)
    assert compute_sma(values) == Decimal("20")


def test_bracket_prices_handles_none_and_pct() -> None:
    sl, tp = bracket_prices(Decimal("100"), None, None)
    assert sl is None and tp is None

    sl, tp = bracket_prices(Decimal("100"), Decimal("0.02"), Decimal("0.05"))
    assert sl == Decimal("98.00")
    assert tp == Decimal("105.00")


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_golden_cross_submits_buy_with_bracket(respx_mock: respx.MockRouter) -> None:
    """Feed a short-below-then-above sequence and assert a BUY is submitted
    with the expected stop_loss / take_profit derived from the signal bar.
    """
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(
            201,
            json={"id": "ord_1", "status": "PENDING"},
        )
    )

    cfg = Config(
        symbol="EUR/USD",
        quantity=Decimal("0.01"),
        short_window=2,
        long_window=3,
        timeframe="1m",
        stop_loss_pct=Decimal("0.02"),
        take_profit_pct=Decimal("0.04"),
    )
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)

        # Warm-up + short below long: closes [1, 2, 3] -> short=2.5, long=2.0
        # Wait — we need short < long first. Use a falling-then-rising sequence.
        # Sequence designed so short_ma crosses BELOW long_ma at bar 3, then
        # ABOVE at bar 5:
        #   bar 1: 5
        #   bar 2: 3   short(maxlen=2)=[5,3]=4, long(maxlen=3)=[5,3]=4 (warmup)
        #   bar 3: 1   short=[3,1]=2,  long=[5,3,1]=3   -> BELOW
        #   bar 4: 4   short=[1,4]=2.5,long=[3,1,4]=2.67 -> BELOW
        #   bar 5: 9   short=[4,9]=6.5,long=[1,4,9]=4.67 -> ABOVE (golden cross)
        for close in ("5", "3", "1", "4", "9"):
            await strat.on_candle(_candle(close))

    assert route.called, "expected a POST /v1/orders on the golden cross"
    import json
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "buy"
    assert body["symbol"] == "EUR/USD"
    assert body["quantity"] == "0.01"
    # SL/TP computed off the signal-bar close (9) with 2% / 4% pct.
    assert body["stop_loss"] == "8.82"
    assert body["take_profit"] == "9.36"
    assert strat._state == StrategyState.PENDING_ENTRY


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_warmup_fires_signal_on_first_live_bar(respx_mock: respx.MockRouter) -> None:
    """Seed SMAs from history so the first live bar can trigger a golden cross.

    Without warm-up the strategy would have to wait `long_window` live bars
    before comparing SMAs. With it, warmup ends with short_ma < long_ma
    (cross=BELOW) and the first live bar flips to short_ma > long_ma — a
    golden cross — which submits a BUY on bar #1.
    """
    bars_route = respx_mock.get("/v1/bars/EUR%2FUSD").mock(
        return_value=httpx.Response(
            200,
            json=[_bar(c) for c in ("5", "3", "1", "4")],
        )
    )
    orders_route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = Config(
        symbol="EUR/USD",
        quantity=Decimal("0.01"),
        short_window=2,
        long_window=3,
        timeframe="1m",
        stop_loss_pct=None,
        take_profit_pct=None,
    )
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        await strat.warm_up()

        # After warmup closes [5, 3, 1, 4]: short=[1,4]=2.5, long=[3,1,4]=2.67
        # → short < long → cross=BELOW.
        assert strat._cross == Cross.BELOW
        assert strat._state == StrategyState.FLAT
        assert bars_route.called

        # First live bar: close=20 → short=[4,20]=12, long=[1,4,20]=8.33
        # → short > long → cross=ABOVE. Golden cross on BELOW→ABOVE fires BUY.
        await strat.on_candle(_candle("20"))

    assert orders_route.called, "expected BUY on the very first live bar"
    assert strat._state == StrategyState.PENDING_ENTRY


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_warmup_falls_back_cleanly_on_fetch_failure(
    respx_mock: respx.MockRouter,
) -> None:
    """A 5xx from the bars endpoint must not kill the strategy — warm_up
    logs and returns, leaving the strategy in cold-start state.
    """
    respx_mock.get("/v1/bars/EUR%2FUSD").mock(
        return_value=httpx.Response(500, json={"code": "ERR", "message": "boom"})
    )

    cfg = Config(
        symbol="EUR/USD",
        quantity=Decimal("0.01"),
        short_window=2,
        long_window=3,
        timeframe="1m",
        stop_loss_pct=None,
        take_profit_pct=None,
    )
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        await strat.warm_up()  # must not raise

    assert len(strat._short) == 0
    assert len(strat._long) == 0
    assert strat._cross == Cross.UNKNOWN
    assert strat._state == StrategyState.FLAT
