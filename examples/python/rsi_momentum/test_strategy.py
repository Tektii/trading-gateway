"""Minimal tests for the RSI momentum template.

Run with: pip install -e .[test] && pytest -q
"""

from __future__ import annotations

import json
from decimal import Decimal

import httpx
import pytest
import respx

from tektii import AsyncTradingGateway, CandleEvent, OrderEvent

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
    equity_fraction: Decimal = Decimal("0.10"),
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
        equity_fraction=equity_fraction,
        period=period,
        oversold=oversold,
        overbought=overbought,
        timeframe=timeframe,
        stop_loss_pct=stop_loss_pct,
        take_profit_pct=take_profit_pct,
    )


def _account(equity: str = "85000") -> dict:
    """A minimal /v1/account payload. ``quantity_for_notional`` reads ``equity``
    to size by fraction; the other fields just satisfy the model.
    """
    return {
        "balance": equity,
        "currency": "USD",
        "equity": equity,
        "margin_available": equity,
        "margin_used": "0",
        "unrealized_pnl": "0",
    }


def _order_event(event: str, side: str, symbol: str = "EUR/USD") -> OrderEvent:
    return OrderEvent.model_validate(
        {
            "type": "order",
            "timestamp": "2026-04-20T10:00:00Z",
            "event": event,
            "order": {
                "id": "ord_1",
                "symbol": symbol,
                "side": side,
                "quantity": "100.00",
                "filled_quantity": "100.00",
                "remaining_quantity": "0",
                "status": "FILLED",
                "order_type": "MARKET",
                "time_in_force": "GTC",
                "created_at": "2026-04-20T10:00:00Z",
                "updated_at": "2026-04-20T10:00:00Z",
            },
        }
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


def test_config_defaults_to_equity_fraction(monkeypatch: pytest.MonkeyPatch) -> None:
    """The template sizes by a fraction of equity, not a fixed unit quantity.

    With nothing configured, ``ORDER_EQUITY_FRACTION`` defaults to 0.10 (10%
    of equity) and there is no longer a fixed ``quantity`` field. A stray
    SYMBOL in the environment is still ignored (the template is
    symbol-agnostic).
    """
    monkeypatch.setenv("SYMBOL", "C:BTCUSD")
    cfg = Config.from_env()
    assert not hasattr(cfg, "symbol")
    assert not hasattr(cfg, "quantity")
    assert cfg.equity_fraction == Decimal("0.10")
    # The other defaults still load.
    assert cfg.period == 14
    assert cfg.oversold == Decimal("30")


def test_config_rejects_non_positive_equity_fraction(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("ORDER_EQUITY_FRACTION", "0")
    with pytest.raises(SystemExit):
        Config.from_env()


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
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account())
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
async def test_oversold_entry_sizes_by_equity_fraction(
    respx_mock: respx.MockRouter,
) -> None:
    """The entry is sized by a fraction of account equity, not a fixed unit
    quantity. With equity 85,000 and the default 10% fraction, the signal-bar
    close of 85 yields qty = 85000 * 0.10 / 85 = 100. The signal close is
    passed as the reference price, so NO quote endpoint is hit (an unmocked
    request would make respx raise).

    SL/TP are still derived from the signal-bar close.
    """
    respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account(equity="85000"))
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
    # equity * fraction / price = 85000 * 0.10 / 85 = 100 (not the old 0.01).
    assert body["quantity"] == "100.00"
    # SL/TP off the signal-bar close (85) with 2% / 5%.
    assert body["stop_loss"] == "83.30"
    assert body["take_profit"] == "89.25"
    assert strat._state == StrategyState.PENDING_ENTRY


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_sells_entered_quantity_not_recomputed(
    respx_mock: respx.MockRouter,
) -> None:
    """Sizing is dynamic per-entry, so the exit must sell the *stored* entry
    quantity, not a quantity recomputed at the exit bar — otherwise it would
    not flatten the position. No account route is registered: a wrongful
    re-size on exit would make respx raise on the unmocked GET.
    """
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = Decimal("100.00")
        await strat._exit_long(symbol="EUR/USD", rsi=Decimal("75"))

    assert route.called, "expected a SELL on the overbought exit"
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "sell"
    assert body["quantity"] == "100.00", "exit must flatten the entered quantity"
    assert strat._state == StrategyState.PENDING_EXIT


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_to_exit_round_trip_sells_entered_quantity(
    respx_mock: respx.MockRouter,
) -> None:
    """Full lifecycle through the state machine: oversold entry stores the
    sized quantity, the BUY fill confirms LONG, and the overbought exit sells
    exactly what was entered (not a re-size at the exit-bar price).

    Uses a non-default fraction so a hardcoded 0.10 would be caught:
    equity 85,000 * 0.25 / signal close 85 = 250 units.
    """
    respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account(equity="85000"))
    )
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = _config(equity_fraction=Decimal("0.25"))
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        # Monotonic decline → RSI = 0 → oversold entry at close=85.
        for close in ("100", "95", "90", "85"):
            await strat.on_candle(_candle(close))
        assert strat._state == StrategyState.PENDING_ENTRY
        assert strat._position_qty == Decimal("250.00")

        # The BUY fill confirms the position.
        strat.on_order(_order_event("ORDER_FILLED", "BUY"))
        assert strat._state == StrategyState.LONG

        # Rally drives RSI from oversold through NEUTRAL into OVERBOUGHT
        # (zone *entry*), firing the exit.
        for close in ("100", "200"):
            await strat.on_candle(_candle(close))

    assert route.call_count == 2, "expected a BUY entry and a SELL exit"
    buy = json.loads(route.calls[0].request.content)
    sell = json.loads(route.calls[-1].request.content)
    assert buy["side"] == "buy"
    assert sell["side"] == "sell"
    assert buy["quantity"] == "250.00"
    assert sell["quantity"] == buy["quantity"], "exit must flatten the entered quantity"
    assert strat._state == StrategyState.PENDING_EXIT


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_reject_keeps_flat(respx_mock: respx.MockRouter) -> None:
    """A rejected entry submit must NOT leave the strategy PENDING_ENTRY —
    a 422 ORDER_REJECTED returns it to FLAT (holding no position quantity)
    so the next oversold entry can retry.
    """
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account())
    )
    respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(
            422, json={"code": "ORDER_REJECTED", "message": "insufficient margin"}
        )
    )
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        await strat._enter_long(
            symbol="EUR/USD", entry_ref=Decimal("85"), rsi=Decimal("25")
        )  # must not raise

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_reject_keeps_long(respx_mock: respx.MockRouter) -> None:
    """A rejected exit submit must NOT pretend the position closed — the
    strategy stays LONG, keeps the stored quantity, and can retry the exit
    on a later bar.
    """
    respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(
            422, json={"code": "ORDER_REJECTED", "message": "market closed"}
        )
    )
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = Decimal("100.00")
        await strat._exit_long(symbol="EUR/USD", rsi=Decimal("75"))  # must not raise

    assert strat._state == StrategyState.LONG
    assert strat._position_qty == Decimal("100.00")


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_skips_on_account_fetch_failure(
    respx_mock: respx.MockRouter,
) -> None:
    """Sizing reads account equity. If that fetch fails (transient outage),
    the entry is skipped best-effort — the strategy stays FLAT rather than
    crashing — mirroring the warm-up fallback.
    """
    account_route = respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(500, json={"code": "ERR", "message": "boom"})
    )
    # No /v1/orders route is registered: if the strategy wrongly submitted an
    # order despite the sizing failure, respx would raise on the unmocked POST.
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        await strat._enter_long(
            symbol="EUR/USD", entry_ref=Decimal("85"), rsi=Decimal("25")
        )  # must not raise

    assert account_route.called
    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_skips_on_non_positive_price(respx_mock: respx.MockRouter) -> None:
    """A degenerate bar (close <= 0) must not crash sizing. Dividing equity by
    a non-positive price is undefined, so the entry is skipped and the strategy
    stays FLAT. No HTTP route is registered: bailing out before sizing means
    neither the account nor the orders endpoint is hit (respx would raise).
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        await strat._enter_long(
            symbol="EUR/USD", entry_ref=Decimal("0"), rsi=Decimal("25")
        )  # must not raise

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_without_tracked_quantity_is_noop(respx_mock: respx.MockRouter) -> None:
    """Defensive guard on the LONG ⟺ tracked-quantity invariant: if an exit is
    somehow requested with no entered quantity recorded, the strategy must not
    submit a null-quantity order — it stays put. No /v1/orders route is
    registered, so a wrongful submit would make respx raise.
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = None
        await strat._exit_long(
            symbol="EUR/USD", rsi=Decimal("75")
        )  # must not raise / must not submit

    assert strat._state == StrategyState.LONG


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_sell_fill_clears_position_qty(respx_mock: respx.MockRouter) -> None:
    """A SELL fill (strategy exit or a bracket SL/TP closing the position)
    must reconcile to FLAT and clear the stored quantity so a later oversold
    entry can re-enter and size fresh.
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.PENDING_EXIT
        strat._position_qty = Decimal("100.00")

        strat.on_order(_order_event("ORDER_FILLED", "SELL"))

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_bracket_sell_fill_while_long_reconciles_to_flat(
    respx_mock: respx.MockRouter,
) -> None:
    """In a *live* run a bracket SL/TP can close the position behind the
    strategy's back — a SELL fill arriving while LONG (not PENDING_EXIT).
    It must reconcile to FLAT and clear the stored quantity so a later
    oversold entry can re-enter. (No-op in backtest, where it never fires.)
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = Decimal("100.00")

        strat.on_order(_order_event("ORDER_FILLED", "SELL"))

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_reject_event_keeps_long_and_quantity(
    respx_mock: respx.MockRouter,
) -> None:
    """An ORDER_REJECTED event while PENDING_EXIT means we are still in the
    market: back to LONG with the stored quantity retained so the exit can
    retry on a later bar.
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.PENDING_EXIT
        strat._position_qty = Decimal("100.00")

        strat.on_order(_order_event("ORDER_REJECTED", "SELL"))

    assert strat._state == StrategyState.LONG
    assert strat._position_qty == Decimal("100.00")


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_reject_event_clears_position_qty(
    respx_mock: respx.MockRouter,
) -> None:
    """An ORDER_REJECTED event while PENDING_ENTRY returns the strategy to
    FLAT and clears the optimistically stored quantity.
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = RsiMomentumStrategy(gw, cfg)
        strat._state = StrategyState.PENDING_ENTRY
        strat._position_qty = Decimal("100.00")

        strat.on_order(_order_event("ORDER_REJECTED", "BUY"))

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


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
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account())
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
