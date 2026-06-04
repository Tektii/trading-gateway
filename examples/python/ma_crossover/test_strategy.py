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
        quantity=Decimal("0.01"),
        short_window=short_window,
        long_window=long_window,
        timeframe=timeframe,
        stop_loss_pct=stop_loss_pct,
        take_profit_pct=take_profit_pct,
    )


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
                "quantity": "0.01",
                "filled_quantity": "0.01",
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


def test_config_is_symbol_agnostic(monkeypatch: pytest.MonkeyPatch) -> None:
    """The template no longer reads a SYMBOL env or carries a symbol field —
    it trades whatever instrument the run is subscribed to. A stray SYMBOL in
    the environment must be ignored, not honoured.
    """
    monkeypatch.setenv("SYMBOL", "C:BTCUSD")
    cfg = Config.from_env()
    assert not hasattr(cfg, "symbol")
    # The other defaults still load.
    assert cfg.short_window == 10
    assert cfg.long_window == 20


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
async def test_golden_cross_submits_buy_with_bracket(respx_mock: respx.MockRouter) -> None:
    """Feed a short-below-then-above sequence and assert a BUY is submitted
    with the expected stop_loss / take_profit derived from the signal bar.
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

    cfg = _config(stop_loss_pct=Decimal("0.02"), take_profit_pct=Decimal("0.04"))
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
    body = json.loads(route.calls.last.request.content)
    assert body["side"] == "buy"
    assert body["symbol"] == "EUR/USD"
    assert body["quantity"] == "0.01"
    # SL/TP computed off the signal-bar close (9) with 2% / 4% pct.
    assert body["stop_loss"] == "8.82"
    assert body["take_profit"] == "9.36"
    # A clean submit transitions to LONG optimistically — the strategy does
    # not wait for an ORDER_FILLED event (the backtest engine never sends one).
    assert strat._state == StrategyState.LONG


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_trades_full_cycle_without_fill_events(
    respx_mock: respx.MockRouter,
) -> None:
    """The regression test for the backtest-never-trades bug.

    The backtest engine does not stream order-lifecycle events back to the
    strategy, so ``on_order`` never fires. The strategy must still complete a
    full FLAT -> LONG -> FLAT cycle, driving both transitions off the order
    submit alone. Feeds a golden cross then a death cross with **no**
    OrderEvent ever delivered and asserts both a BUY and a SELL are submitted.
    """
    respx_mock.get(url__regex=r".*/v1/bars/.*").mock(
        return_value=httpx.Response(200, json=[])
    )
    route = respx_mock.post("/v1/orders").mock(
        return_value=httpx.Response(201, json={"id": "ord_1", "status": "PENDING"}),
    )

    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)

        # short(maxlen=2), long(maxlen=3):
        #   5            warmup
        #   3            short=[5,3]=4,  long=[5,3]=4      (warmup)
        #   1            short=[3,1]=2,  long=[5,3,1]=3    -> BELOW
        #   4            short=[1,4]=2.5,long=[3,1,4]=2.67 -> BELOW
        #   9            short=[4,9]=6.5,long=[1,4,9]=4.67 -> ABOVE  golden -> BUY
        #   1            short=[9,1]=5,  long=[4,9,1]=4.67 -> ABOVE
        #   1            short=[1,1]=1,  long=[9,1,1]=3.67 -> BELOW  death -> SELL
        for close in ("5", "3", "1", "4", "9", "1", "1"):
            await strat.on_candle(_candle(close))

    assert route.call_count == 2, "expected a BUY on the golden cross and a SELL on the death cross"
    first = json.loads(route.calls[0].request.content)
    last = json.loads(route.calls[-1].request.content)
    assert first["side"] == "buy"
    assert last["side"] == "sell"
    assert strat._state == StrategyState.FLAT


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_bracket_sell_fill_reconciles_to_flat(
    respx_mock: respx.MockRouter,
) -> None:
    """In a *live* run a bracket SL/TP can close the position behind the
    strategy's back. The resulting SELL fill must reconcile LONG -> FLAT so a
    later golden cross can re-enter. (No-op in backtest, where it never fires.)
    """
    cfg = _config()
    async with AsyncTradingGateway(base_url="http://localhost:8080") as gw:
        strat = MaCrossoverStrategy(gw, cfg)
        strat._state = StrategyState.LONG

        strat.on_order(_order_event("ORDER_FILLED", "SELL"))

    assert strat._state == StrategyState.FLAT


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_entry_reject_keeps_flat(respx_mock: respx.MockRouter) -> None:
    """A rejected entry submit must NOT optimistically transition to LONG.

    This is the safety counterweight to the optimistic model: state only
    flips on a *clean* submit. A 422 ORDER_REJECTED leaves the strategy FLAT
    so the next golden cross can retry.
    """
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


@pytest.mark.asyncio
@respx.mock(base_url="http://localhost:8080")
async def test_exit_reject_keeps_long(respx_mock: respx.MockRouter) -> None:
    """A rejected exit submit must NOT optimistically transition to FLAT —
    we are still in the market, so the strategy stays LONG and can retry the
    exit on a later bar.
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
        await strat._exit_long(symbol="EUR/USD")  # must not raise

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

        strat.on_order(_order_event("ORDER_FILLED", "BUY"))

    assert strat._state == StrategyState.LONG


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
