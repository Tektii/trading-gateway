"""Minimal tests for the MA crossover template.

Run with: pip install -e .[test] && pytest -q
"""

from __future__ import annotations

from collections import deque
from datetime import UTC, datetime
from decimal import Decimal

import httpx
import pytest
import respx

from tektii import AsyncTektiiGateway, CandleEvent

from strategy import (
    Config,
    MaCrossoverStrategy,
    StrategyState,
    bracket_prices,
    compute_sma,
)


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
        stop_loss_pct=Decimal("0.02"),
        take_profit_pct=Decimal("0.04"),
    )
    async with AsyncTektiiGateway(base_url="http://localhost:8080") as gw:
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
