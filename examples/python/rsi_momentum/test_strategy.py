"""Minimal tests for the RSI momentum template.

Run with: pip install -e .[test] && pytest -q
"""

from __future__ import annotations

import json
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


def _config(
    *,
    period: int = 3,
    oversold: Decimal = Decimal("30"),
    overbought: Decimal = Decimal("70"),
    timeframe: str = "1m",
    stop_loss_pct: Decimal | None = None,
    take_profit_pct: Decimal | None = None,
) -> Config:
    """A test Config. The template is symbol-agnostic, so there is no symbol
    to configure — it trades whatever stream it receives.
    """
    return Config(
        quantity=Decimal("0.01"),
        period=period,
        oversold=oversold,
        overbought=overbought,
        timeframe=timeframe,
        stop_loss_pct=stop_loss_pct,
        take_profit_pct=take_profit_pct,
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


def test_config_is_symbol_agnostic(monkeypatch: pytest.MonkeyPatch) -> None:
    """The template no longer reads a SYMBOL env or carries a symbol field —
    it trades whatever instrument the run is subscribed to. A stray SYMBOL in
    the environment must be ignored, not honoured.
    """
    monkeypatch.setenv("SYMBOL", "C:BTCUSD")
    cfg = Config.from_env()
    assert not hasattr(cfg, "symbol")
    # The other defaults still load.
    assert cfg.period == 14
    assert cfg.oversold == Decimal("30")


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_trades_on_subscribed_symbol_without_config(
    respx_mock: respx.MockRouter,
) -> None:
    """Regression for the silent no-trade bug: with no symbol configured, the
    strategy must trade whatever instrument the stream delivers — here a
    crypto symbol that never matched the old `F:EURUSD` default — and submit
    the order on that same symbol.
    """
    # Warm-up fetches history on the stream's symbol; return none so the
    # strategy cold-starts off the live bars below.
    bars_route = respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        # Monotonic decline → RSI = 0 → oversold entry, on a symbol the old
        # default would have filtered out entirely.
        for close in ("100", "95", "90", "85"):
            await strat.on_candle(_candle(close, symbol="C:BTCUSD"))

    assert route.called, "expected a BUY even though no symbol was configured"
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "buy"
    assert body["symbol"] == "C:BTCUSD", "order must target the stream's symbol"
    assert strat._state == StrategyState.PENDING_ENTRY
    # Lazy warm-up must fire exactly once, not on every candle.
    assert bars_route.call_count == 1


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_oversold_entry_submits_buy_with_bracket(respx_mock: respx.MockRouter) -> None:
    """A monotonic price decline drives RSI into the oversold zone and
    triggers a BUY with the expected SL/TP computed from the signal bar.
    """
    respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(
            201,
            json={"id": "ord_1", "status": "PENDING"},
        )
    )

    cfg = _config(stop_loss_pct=Decimal("0.02"), take_profit_pct=Decimal("0.05"))
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        for close in ("100", "95", "90", "85"):
            await strat.on_candle(_candle(close))

    assert route.called, "expected a POST /v1/orders on oversold entry"
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
    """Warm-up runs lazily on the first candle, seeding the Wilder RSI from
    history on that candle's symbol so the first live bar can already fire.

    History is a mixed sequence that leaves the Wilder state in the NEUTRAL
    zone after warmup. The first live bar is a sharp drop that pulls RSI into
    OVERSOLD — which fires a BUY on bar #1 instead of waiting `period + 1`
    live bars.
    """
    bars_route = respx_mock.get("/v1/bars/EUR%2FUSD").mock(
        return_value=httpx.Response(
            200,
            # Alternating gains/losses leave RSI ~mid-band → NEUTRAL.
            json=[_bar(c) for c in ("100", "101", "100", "101")],
        )
    )
    orders_route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)

        # First live bar both triggers warm-up (history fetch, ending NEUTRAL)
        # and processes a sharp drop (close=10) → RSI into OVERSOLD → BUY.
        await strat.on_candle(_candle("10"))

    assert bars_route.called, "expected lazy warm-up to fetch history on the first candle"
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

    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        await strat.warm_up("EUR/USD")  # must not raise

    assert strat._zone == Zone.UNKNOWN
    assert strat._state == StrategyState.FLAT
