"""Minimal tests for the RSI momentum template.

Run with: pip install -e .[test] && pytest -q
"""

from __future__ import annotations

from decimal import Decimal

import httpx
import pytest
import respx

from tektii import AsyncTradingGateway, CandleEvent

from strategy import (
    Config,
    RsiMomentumStrategy,
    RsiState,
    StrategyState,
    Zone,
    _rsi_from_avgs,
    bracket_prices,
    classify_zone,
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


def test_rsi_all_gains_saturates_at_100() -> None:
    # With avg_loss == 0, the guard returns 100 without dividing.
    assert _rsi_from_avgs(Decimal("5"), Decimal("0")) == Decimal(100)


def test_rsi_state_produces_zero_on_monotonic_decline() -> None:
    rsi = RsiState(period=3)
    # 3+1 closes declining by a constant: all losses, zero gains -> RSI = 0.
    assert rsi.update(Decimal("100")) is None
    assert rsi.update(Decimal("95")) is None
    assert rsi.update(Decimal("90")) is None
    value = rsi.update(Decimal("85"))
    assert value == Decimal(0)


def test_classify_zone_boundaries() -> None:
    oversold, overbought = Decimal("30"), Decimal("70")
    assert classify_zone(Decimal("25"), oversold, overbought) == Zone.OVERSOLD
    assert classify_zone(Decimal("30"), oversold, overbought) == Zone.OVERSOLD
    assert classify_zone(Decimal("50"), oversold, overbought) == Zone.NEUTRAL
    assert classify_zone(Decimal("70"), oversold, overbought) == Zone.OVERBOUGHT
    assert classify_zone(Decimal("85"), oversold, overbought) == Zone.OVERBOUGHT


def test_bracket_prices_handles_none_and_pct() -> None:
    sl, tp = bracket_prices(Decimal("100"), None, None)
    assert sl is None and tp is None

    sl, tp = bracket_prices(Decimal("100"), Decimal("0.02"), Decimal("0.05"))
    assert sl == Decimal("98.00")
    assert tp == Decimal("105.00")


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_oversold_entry_submits_buy_with_bracket(respx_mock: respx.MockRouter) -> None:
    """A monotonic price decline drives RSI into the oversold zone and
    triggers a BUY with the expected SL/TP computed from the signal bar.
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
        period=3,
        oversold=Decimal("30"),
        overbought=Decimal("70"),
        timeframe="1m",
        stop_loss_pct=Decimal("0.02"),
        take_profit_pct=Decimal("0.05"),
    )
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        for close in ("100", "95", "90", "85"):
            await strat.on_candle(_candle(close))

    assert route.called, "expected a POST /v1/orders on oversold entry"
    import json
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "buy"
    assert body["symbol"] == "EUR/USD"
    assert body["quantity"] == "0.01"
    # SL/TP off the signal-bar close (85) with 2% / 5%.
    assert body["stop_loss"] == "83.30"
    assert body["take_profit"] == "89.25"
    assert strat._state == StrategyState.PENDING_ENTRY


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_warmup_fires_signal_on_first_live_bar(respx_mock: respx.MockRouter) -> None:
    """Seed RSI from history so the first live bar can produce an entry.

    History is a 4-bar rise that leaves the Wilder state in the NEUTRAL
    zone after warmup. The first live bar is a sharp drop that pulls RSI
    into OVERSOLD — which fires a BUY on bar #1 instead of waiting
    `period + 1` live bars.
    """
    bars_route = respx_mock.get("/v1/bars/EUR%2FUSD").mock(
        return_value=httpx.Response(
            200,
            # Monotonic rise: all gains, zero losses → RSI saturates at 100
            # → zone NEUTRAL only if RSI lands above oversold and below
            # overbought. Rising gives RSI=100 (OVERBOUGHT). Use a mixed
            # sequence ending NEUTRAL instead.
            json=[_bar(c) for c in ("100", "101", "100", "101")],
        )
    )
    orders_route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = Config(
        symbol="EUR/USD",
        quantity=Decimal("0.01"),
        period=3,
        oversold=Decimal("30"),
        overbought=Decimal("70"),
        timeframe="1m",
        stop_loss_pct=None,
        take_profit_pct=None,
    )
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        await strat.warm_up()

        # After warmup: RSI computed; zone should be NEUTRAL (RSI ~50
        # from alternating gains/losses).
        assert bars_route.called
        assert strat._zone == Zone.NEUTRAL
        assert strat._state == StrategyState.FLAT

        # First live bar: a sharp drop takes RSI into OVERSOLD → BUY.
        await strat.on_candle(_candle("10"))

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
        period=3,
        oversold=Decimal("30"),
        overbought=Decimal("70"),
        timeframe="1m",
        stop_loss_pct=None,
        take_profit_pct=None,
    )
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        await strat.warm_up()  # must not raise

    assert strat._zone == Zone.UNKNOWN
    assert strat._state == StrategyState.FLAT
