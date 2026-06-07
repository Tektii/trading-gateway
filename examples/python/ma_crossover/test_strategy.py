"""Minimal tests for the MA crossover template.

Run with: pip install -e .[test] && pytest -q
"""

from __future__ import annotations

import json
from collections import deque
from decimal import Decimal

import httpx
import pytest
import respx

from tektii import AsyncTradingGateway, CandleEvent, OrderEvent

from strategy import (
    Config,
    Cross,
    MaCrossoverStrategy,
    StrategyState,
    bracket_prices,
    compute_sma,
)


def _config(
    *,
    equity_fraction: Decimal = Decimal("0.10"),
    short_window: int = 2,
    long_window: int = 3,
    timeframe: str = "1m",
    stop_loss_pct: Decimal | None = None,
    take_profit_pct: Decimal | None = None,
) -> Config:
    """A test Config. The template is symbol-agnostic, so there is no symbol
    to configure — it trades whatever stream it receives.
    """
    return Config(
        equity_fraction=equity_fraction,
        short_window=short_window,
        long_window=long_window,
        timeframe=timeframe,
        stop_loss_pct=stop_loss_pct,
        take_profit_pct=take_profit_pct,
    )


def _account(equity: str = "90000") -> dict:
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
                "quantity": "1000.00",
                "filled_quantity": "1000.00",
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


def test_compute_sma_matches_manual_average() -> None:
    values = deque([Decimal("10"), Decimal("20"), Decimal("30")], maxlen=3)
    assert compute_sma(values) == Decimal("20")


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
    assert cfg.short_window == 10
    assert cfg.long_window == 20


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
        strat = MaCrossoverStrategy(gw, cfg)
        # Same golden-cross sequence as the EUR/USD test, but on a symbol the
        # old default would have filtered out entirely.
        for close in ("5", "3", "1", "4", "9"):
            await strat.on_candle(_candle(close, symbol="C:BTCUSD"))

    assert route.called, "expected a BUY even though no symbol was configured"
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "buy"
    assert body["symbol"] == "C:BTCUSD", "order must target the stream's symbol"
    assert strat._state == StrategyState.LONG
    # Lazy warm-up must fire exactly once, not on every candle.
    assert bars_route.call_count == 1


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_golden_cross_sizes_by_equity_fraction(
    respx_mock: respx.MockRouter,
) -> None:
    """The entry is sized by a fraction of account equity, not a fixed unit
    quantity. With equity 90,000 and the default 10% fraction, the signal-bar
    close of 9 yields qty = 90000 * 0.10 / 9 = 1000. The signal close is
    passed as the reference price, so NO quote endpoint is hit (an unmocked
    request would make respx raise).

    SL/TP are still derived from the signal-bar close.
    """
    respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account(equity="90000"))
    )
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"})
    )

    cfg = _config(stop_loss_pct=Decimal("0.02"), take_profit_pct=Decimal("0.04"))
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        # Golden cross at the last bar (close=9) — see other tests for the
        # SMA arithmetic on this sequence.
        for close in ("5", "3", "1", "4", "9"):
            await strat.on_candle(_candle(close))

    assert route.called, "expected a POST /v1/orders on the golden cross"
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "buy"
    assert body["symbol"] == "EUR/USD"
    # equity * fraction / price = 90000 * 0.10 / 9 = 1000 (not the old 0.01).
    assert body["quantity"] == "1000.00"
    # SL/TP computed off the signal-bar close (9) with 2% / 4% pct.
    assert body["stop_loss"] == "8.82"
    assert body["take_profit"] == "9.36"
    assert strat._state == StrategyState.LONG


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_sells_entered_quantity_not_recomputed(
    respx_mock: respx.MockRouter,
) -> None:
    """Sizing is dynamic per-entry, so the exit must sell the *stored* entry
    quantity, not a quantity recomputed from the exit-bar price — otherwise it
    would not flatten the position.

    Entry at close=9 with equity 90,000 / 10% sizes 1000 units. The death
    cross arrives at close=1; recomputing there would size 9000, so asserting
    the SELL is 1000 proves we sell what we hold.
    """
    respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    respx_mock.get("/v1/account").mock(
        return_value=httpx.Response(200, json=_account(equity="90000"))
    )
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        # Golden cross -> BUY at close=9, then death cross -> SELL at close=1.
        for close in ("5", "3", "1", "4", "9", "1", "1"):
            await strat.on_candle(_candle(close))

    assert route.call_count == 2, "expected a BUY on the golden cross and a SELL on the death cross"
    buy = json.loads(route.calls[0].request.content)
    sell = json.loads(route.calls[-1].request.content)
    assert buy["side"] == "buy"
    assert sell["side"] == "sell"
    assert buy["quantity"] == "1000.00"
    assert sell["quantity"] == buy["quantity"], "exit must flatten the entered quantity"
    assert strat._state == StrategyState.FLAT


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_bracket_sell_fill_reconciles_to_flat(
    respx_mock: respx.MockRouter,
) -> None:
    """In a *live* run a bracket SL/TP can close the position behind the
    strategy's back. The resulting SELL fill must reconcile LONG -> FLAT and
    clear the stored position quantity so a later golden cross can re-enter.
    (No-op in backtest, where it never fires.)
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = Decimal("1000.00")

        strat.on_order(_order_event("ORDER_FILLED", "SELL"))

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_reject_keeps_flat(respx_mock: respx.MockRouter) -> None:
    """A rejected entry submit must NOT optimistically transition to LONG.

    This is the safety counterweight to the optimistic model: state only
    flips on a *clean* submit. A 422 ORDER_REJECTED leaves the strategy FLAT
    (and holding no position quantity) so the next golden cross can retry.
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
        strat = MaCrossoverStrategy(gw, cfg)
        # _enter_long takes the symbol from the live candle; supply one here.
        await strat._enter_long(symbol="EUR/USD", entry_ref=Decimal("100"))  # must not raise

    assert strat._state == StrategyState.FLAT
    assert strat._position_qty is None


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_reject_keeps_long(respx_mock: respx.MockRouter) -> None:
    """A rejected exit submit must NOT optimistically transition to FLAT —
    we are still in the market, so the strategy stays LONG, keeps the stored
    quantity, and can retry the exit on a later bar.
    """
    respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(
            422, json={"code": "ORDER_REJECTED", "message": "market closed"}
        )
    )
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = Decimal("1000.00")
        await strat._exit_long(symbol="EUR/USD")  # must not raise

    assert strat._state == StrategyState.LONG
    assert strat._position_qty == Decimal("1000.00")


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
        strat = MaCrossoverStrategy(gw, cfg)
        await strat._enter_long(symbol="EUR/USD", entry_ref=Decimal("9"))  # must not raise

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
        strat = MaCrossoverStrategy(gw, cfg)
        await strat._enter_long(symbol="EUR/USD", entry_ref=Decimal("0"))  # must not raise

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
        strat = MaCrossoverStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = None
        await strat._exit_long(symbol="EUR/USD")  # must not raise / must not submit

    assert strat._state == StrategyState.LONG


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_buy_fill_while_long_does_not_flip_state(
    respx_mock: respx.MockRouter,
) -> None:
    """on_order only reconciles SELL fills. A BUY fill arriving while LONG
    (e.g. an echoed entry event in live) must leave the position untouched.
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        strat._state = StrategyState.LONG
        strat._position_qty = Decimal("1000.00")

        strat.on_order(_order_event("ORDER_FILLED", "BUY"))

    assert strat._state == StrategyState.LONG
    assert strat._position_qty == Decimal("1000.00")


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_warmup_fires_signal_on_first_live_bar(respx_mock: respx.MockRouter) -> None:
    """Warm-up runs lazily on the first candle, seeding the SMAs from history
    on that candle's symbol so the very first live bar can already signal.

    History closes [5, 3, 1, 4] leave short=[1,4]=2.5 < long=[3,1,4]=2.67
    (cross=BELOW). The first live bar (close=20) flips short>long — a golden
    cross — which submits a BUY on bar #1 instead of waiting `long_window`
    live bars.
    """
    bars_route = respx_mock.get("/v1/bars/EUR%2FUSD").mock(
        return_value=httpx.Response(
            200,
            json=[_bar(c) for c in ("5", "3", "1", "4")],
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
        strat = MaCrossoverStrategy(gw, cfg)

        # First live bar both triggers warm-up (history fetch) and processes
        # close=20 → short=[4,20]=12 > long=[1,4,20]=8.33 → golden cross → BUY.
        await strat.on_candle(_candle("20"))

    assert bars_route.called, "expected lazy warm-up to fetch history on the first candle"
    assert orders_route.called, "expected BUY on the very first live bar"
    assert strat._state == StrategyState.LONG


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
        strat = MaCrossoverStrategy(gw, cfg)
        await strat.warm_up("EUR/USD")  # must not raise

    assert len(strat._short) == 0
    assert len(strat._long) == 0
    assert strat._cross == Cross.UNKNOWN
    assert strat._state == StrategyState.FLAT
