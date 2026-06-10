"""RSI Mean Reversion — Tektii platform reference strategy.

A mean-reversion strategy that opens a long position when the Relative
Strength Index enters the oversold zone (by default RSI < 30) and closes it
when RSI enters the overbought zone (by default RSI > 70). Signals fire on
*zone entry*, not while in-zone, so the strategy doesn't over-trade.

Designed to be the canonical shape for strategies on the Tektii platform:
stateful class, pure indicator helpers, SDK-native event dispatch, and a
small state machine that survives partial fills and broker rejects.

The strategy is **symbol-agnostic**: it trades whatever instrument the run is
subscribed to, learned from the incoming stream — there is nothing to
configure and nothing to keep in sync. It keeps a single set of indicator
state, so subscribe it to one instrument at a time.

Environment variables
---------------------
===================  ==============  ==============================================
Name                 Default         Description
===================  ==============  ==============================================
ORDER_EQUITY_FRACTION 0.10           Position size as a fraction of account equity
                                     (0.10 = 10%). Sized at the signal-bar price
                                     each entry — see "Position sizing" below
RSI_PERIOD           14              RSI lookback, in bars (Wilder's smoothing)
RSI_OVERSOLD         30              Enter long when RSI crosses below this
RSI_OVERBOUGHT       70              Exit long when RSI crosses above this
TIMEFRAME            1m              Bar resolution used for the warm-up backfill;
                                     should match the gateway stream resolution
STOP_LOSS_PCT        (unset)         e.g. "0.02" = attach 2% SL to entries
TAKE_PROFIT_PCT      (unset)         e.g. "0.04" = attach 4% TP to entries
LOG_LEVEL            INFO            Python logging level
TRADING_GATEWAY_URL   localhost:8080  Gateway base URL (read by the SDK)
TRADING_GATEWAY_API_KEY       (unset)         API key for remote gateways (read by the SDK)
===================  ==============  ==============================================

Position sizing
---------------
Each entry trades a **fraction of account equity** — ``ORDER_EQUITY_FRACTION``
(default 0.10 = 10%). The size scales with the account, so the same fraction
trades a sensible position at any capital and on any instrument.

The SDK helper ``gw.quantity_for_notional`` (``tektii`` >= 1.6.0) does the
maths: it reads account equity and divides by a reference price. Pass the bar
close as ``price`` to use it directly and skip a separate quote request::

    qty = await gw.quantity_for_notional(
        bar.symbol, equity_fraction="0.10", price=bar.close
    )

To trade a fixed cash amount instead, pass ``notional="5000"`` in place of
``equity_fraction``.

Because the size depends on the entry price, the position quantity is stored on
entry and sold in full on the overbought exit.

The helper returns a full-precision quantity. The mock provider used in
backtests accepts it as-is; real brokers (Alpaca, Binance, Oanda) require it
rounded to the instrument's lot size before submitting.

Local run
---------
    pip install -e .
    python strategy.py

Docker
------
    docker build -t tektii-template-rsi-momentum:dev .
    docker run --rm --network=host tektii-template-rsi-momentum:dev
"""

from __future__ import annotations

import asyncio
import logging
import os
import signal
from dataclasses import dataclass
from decimal import Decimal
from enum import Enum, auto

from tektii import (
    AsyncTradingGateway,
    CandleEvent,
    ConnectionEvent,
    ErrorEvent,
    OrderEvent,
    OrderRejectedError,
    TektiiError,
)

# Extra bars fetched beyond the minimum needed for the longest indicator
# window — cushions against missing/duplicate/partial bars at the tail
# of the provider's history.
WARMUP_MARGIN_BARS = 5

log = logging.getLogger("rsi_momentum")


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Config:
    equity_fraction: Decimal
    period: int
    oversold: Decimal
    overbought: Decimal
    timeframe: str
    stop_loss_pct: Decimal | None
    take_profit_pct: Decimal | None

    @classmethod
    def from_env(cls) -> Config:
        def opt_decimal(name: str) -> Decimal | None:
            raw = os.environ.get(name)
            return Decimal(raw) if raw else None

        try:
            cfg = cls(
                equity_fraction=Decimal(os.environ.get("ORDER_EQUITY_FRACTION", "0.10")),
                period=int(os.environ.get("RSI_PERIOD", "14")),
                oversold=Decimal(os.environ.get("RSI_OVERSOLD", "30")),
                overbought=Decimal(os.environ.get("RSI_OVERBOUGHT", "70")),
                timeframe=os.environ.get("TIMEFRAME", "1m"),
                stop_loss_pct=opt_decimal("STOP_LOSS_PCT"),
                take_profit_pct=opt_decimal("TAKE_PROFIT_PCT"),
            )
        except (ValueError, ArithmeticError) as err:
            raise SystemExit(f"Invalid configuration: {err}") from err
        if cfg.equity_fraction <= 0:
            raise SystemExit("ORDER_EQUITY_FRACTION must be positive")
        if cfg.period <= 1:
            raise SystemExit("RSI_PERIOD must be > 1")
        if not (Decimal(0) < cfg.oversold < cfg.overbought < Decimal(100)):
            raise SystemExit(
                "Require 0 < RSI_OVERSOLD < RSI_OVERBOUGHT < 100 "
                f"(got {cfg.oversold}, {cfg.overbought})"
            )
        return cfg


# ---------------------------------------------------------------------------
# State machine
# ---------------------------------------------------------------------------


class StrategyState(Enum):
    """Where the strategy is in its single-position lifecycle.

    Using an enum instead of `has_position: bool + pending_order: bool`
    avoids the ambiguous state where both are set (rejected order races,
    cancel-before-ack) and makes the transitions explicit.
    """

    FLAT = auto()
    PENDING_ENTRY = auto()
    LONG = auto()
    PENDING_EXIT = auto()


class Zone(Enum):
    """Which RSI band the previous bar sat in."""

    UNKNOWN = auto()
    OVERSOLD = auto()
    NEUTRAL = auto()
    OVERBOUGHT = auto()


# ---------------------------------------------------------------------------
# Indicator (Wilder's RSI, pure & unit-testable)
# ---------------------------------------------------------------------------


class RsiState:
    """Incremental Wilder's-smoothing RSI.

    The standard formulation: seed ``avg_gain`` / ``avg_loss`` with the
    simple mean of the first ``period`` gains and losses (computed from
    ``period + 1`` closes), then apply Wilder smoothing thereafter:

        avg = (prev_avg * (period - 1) + current) / period

    Guards against the degenerate ``avg_loss == 0`` case by returning
    RSI = 100 directly rather than dividing.
    """

    def __init__(self, period: int) -> None:
        if period <= 1:
            raise ValueError("period must be > 1")
        self._period = period
        self._prev_close: Decimal | None = None
        self._gains: list[Decimal] = []
        self._losses: list[Decimal] = []
        self._avg_gain: Decimal | None = None
        self._avg_loss: Decimal | None = None

    def update(self, close: Decimal) -> Decimal | None:
        """Feed the next close. Returns the current RSI once warm, else None."""
        if self._prev_close is None:
            self._prev_close = close
            return None

        change = close - self._prev_close
        self._prev_close = close
        gain = change if change > 0 else Decimal(0)
        loss = -change if change < 0 else Decimal(0)

        if self._avg_gain is None:
            self._gains.append(gain)
            self._losses.append(loss)
            if len(self._gains) < self._period:
                return None
            # Seed: SMA of the first `period` gains and losses.
            self._avg_gain = sum(self._gains, start=Decimal(0)) / Decimal(self._period)
            self._avg_loss = sum(self._losses, start=Decimal(0)) / Decimal(self._period)
            self._gains.clear()
            self._losses.clear()
        else:
            # Wilder smoothing.
            assert self._avg_loss is not None
            n = Decimal(self._period)
            self._avg_gain = (self._avg_gain * (n - 1) + gain) / n
            self._avg_loss = (self._avg_loss * (n - 1) + loss) / n

        return _rsi_from_avgs(self._avg_gain, self._avg_loss)


def _rsi_from_avgs(avg_gain: Decimal, avg_loss: Decimal) -> Decimal:
    """RSI = 100 - 100 / (1 + avg_gain / avg_loss), with avg_loss=0 guard."""
    if avg_loss == 0:
        return Decimal(100)
    rs = avg_gain / avg_loss
    return Decimal(100) - Decimal(100) / (Decimal(1) + rs)


def classify_zone(rsi: Decimal, oversold: Decimal, overbought: Decimal) -> Zone:
    if rsi <= oversold:
        return Zone.OVERSOLD
    if rsi >= overbought:
        return Zone.OVERBOUGHT
    return Zone.NEUTRAL


def bracket_prices(
    entry: Decimal,
    stop_loss_pct: Decimal | None,
    take_profit_pct: Decimal | None,
) -> tuple[Decimal | None, Decimal | None]:
    """SL/TP prices for a LONG entry from a reference price.

    Uses the signal-bar close as the reference — a known simplification,
    since the market fill is on the next bar's open. For a tighter template,
    wait for ORDER_FILLED and read ``order.average_fill_price`` before
    placing the bracket legs.

    Note: the raw multiplication returns full-precision ``Decimal`` values.
    The mock provider accepts these, but real brokers (Alpaca, Binance,
    Oanda) reject prices that violate an instrument's tick size. Before
    submitting against a real broker, ``Decimal.quantize(...)`` the SL/TP to
    the instrument's tick.
    """
    sl = entry * (Decimal(1) - stop_loss_pct) if stop_loss_pct is not None else None
    tp = entry * (Decimal(1) + take_profit_pct) if take_profit_pct is not None else None
    return sl, tp


# ---------------------------------------------------------------------------
# Strategy
# ---------------------------------------------------------------------------


class RsiMomentumStrategy:
    def __init__(self, gw: AsyncTradingGateway, cfg: Config) -> None:
        self._gw = gw
        self._cfg = cfg
        self._rsi = RsiState(cfg.period)
        self._zone = Zone.UNKNOWN
        self._state = StrategyState.FLAT
        self._warmed_up = False
        # Quantity of the open position, set on a clean entry submit and sold
        # in full on exit. None while flat.
        self._position_qty: Decimal | None = None

    async def warm_up(self, symbol: str) -> None:
        """Seed the Wilder RSI state from historical bars for ``symbol``.

        Runs lazily on the first candle (see ``on_candle``): the strategy is
        symbol-agnostic, so it only learns which instrument to back-fill once
        the stream delivers the first bar.

        Without this, ``RsiState`` returns None for the first ``period + 1``
        live bars. With it, the first live bar already produces an RSI value
        and can fire a zone-entry signal.

        Best-effort: any SDK error (connection, auth, 5xx, bad timeframe)
        falls back to cold-start with a warning so a transient history
        outage doesn't kill the strategy on boot.
        """
        limit = self._cfg.period + 1 + WARMUP_MARGIN_BARS
        try:
            bars = await self._gw.get_bars(
                symbol, self._cfg.timeframe, limit=limit
            )
        except TektiiError as err:
            log.warning(
                "history fetch failed, falling back to cold start: %s", err
            )
            return

        if not bars:
            log.warning(
                "history fetch returned no bars, falling back to cold start"
            )
            return

        last_rsi: Decimal | None = None
        for bar in bars:
            value = self._rsi.update(Decimal(bar.close))
            if value is not None:
                last_rsi = value

        if last_rsi is not None:
            self._zone = classify_zone(
                last_rsi, self._cfg.oversold, self._cfg.overbought
            )

        log.info(
            "warmed up from history: %d bars, rsi=%s zone=%s",
            len(bars), last_rsi, self._zone.name,
        )

    async def on_candle(self, event: CandleEvent) -> None:
        bar = event.bar

        # Symbol-agnostic: trade whatever instrument the run is subscribed to.
        # The first candle reveals the symbol, so warm-up runs here (once)
        # rather than before the stream opens.
        if not self._warmed_up:
            self._warmed_up = True
            await self.warm_up(bar.symbol)

        close = Decimal(bar.close)
        rsi = self._rsi.update(close)
        if rsi is None:
            return  # still warming up

        new_zone = classify_zone(rsi, self._cfg.oversold, self._cfg.overbought)
        log.debug(
            "bar close=%s rsi=%s zone=%s state=%s",
            close, rsi, new_zone.name, self._state.name,
        )

        prev = self._zone
        self._zone = new_zone

        # Fire on zone ENTRY, not while in-zone.
        if new_zone == Zone.OVERSOLD and prev != Zone.OVERSOLD and self._state == StrategyState.FLAT:
            await self._enter_long(symbol=bar.symbol, entry_ref=close, rsi=rsi)
        elif (
            new_zone == Zone.OVERBOUGHT
            and prev != Zone.OVERBOUGHT
            and self._state == StrategyState.LONG
        ):
            await self._exit_long(symbol=bar.symbol, rsi=rsi)

    async def _enter_long(self, *, symbol: str, entry_ref: Decimal, rsi: Decimal) -> None:
        # Sizing divides equity by the price, so a non-positive price has no
        # valid size. Skip the entry.
        if entry_ref <= 0:
            log.warning(
                "non-positive reference price %s on %s, skipping entry",
                entry_ref, symbol,
            )
            return
        # Size the order as a fraction of account equity. Passing the bar close
        # as ``price`` avoids a separate quote request. If sizing fails (e.g.
        # the account fetch errors), skip this entry instead of crashing.
        try:
            quantity = await self._gw.quantity_for_notional(
                symbol, equity_fraction=self._cfg.equity_fraction, price=entry_ref
            )
        except TektiiError as err:
            log.warning("position sizing failed, skipping entry: %s", err)
            return  # could not size: stay FLAT
        sl, tp = bracket_prices(
            entry_ref, self._cfg.stop_loss_pct, self._cfg.take_profit_pct
        )
        self._state = StrategyState.PENDING_ENTRY
        log.info(
            "oversold on %s rsi=%s: submitting BUY qty=%s sl=%s tp=%s",
            symbol, rsi, quantity, sl, tp,
        )
        try:
            handle = await self._gw.submit_order(
                symbol=symbol,
                side="buy",
                quantity=quantity,
                stop_loss=sl,
                take_profit=tp,
            )
            # Submit accepted — record the quantity so the exit can sell the
            # whole position.
            self._position_qty = quantity
            log.info("order accepted id=%s status=%s", handle.id, handle.status)
        except OrderRejectedError as err:
            log.warning("order rejected code=%s message=%s", err.code, err.message)
            self._state = StrategyState.FLAT

    async def _exit_long(self, *, symbol: str, rsi: Decimal) -> None:
        quantity = self._position_qty
        # A LONG position always has a recorded quantity; guard against selling
        # an unknown size.
        if quantity is None:
            log.error("exit requested on %s with no tracked quantity; staying LONG", symbol)
            return
        self._state = StrategyState.PENDING_EXIT
        log.info(
            "overbought on %s rsi=%s: submitting SELL qty=%s",
            symbol, rsi, quantity,
        )
        try:
            handle = await self._gw.submit_order(
                symbol=symbol,
                side="sell",
                quantity=quantity,
            )
            log.info("order accepted id=%s status=%s", handle.id, handle.status)
        except OrderRejectedError as err:
            log.warning("order rejected code=%s message=%s", err.code, err.message)
            self._state = StrategyState.LONG

    def on_order(self, event: OrderEvent) -> None:
        order = event.order
        log.info(
            "order %s id=%s symbol=%s side=%s qty=%s filled=%s status=%s",
            event.event, order.id, order.symbol, order.side,
            order.quantity, order.filled_quantity, order.status,
        )
        # Bracket SL/TP child orders are managed by the gateway — we only
        # drive our own state from the parent entry/exit orders.
        if event.event == "ORDER_FILLED":
            if order.side == "BUY" and self._state == StrategyState.PENDING_ENTRY:
                self._state = StrategyState.LONG
            elif order.side == "SELL" and self._state in (
                StrategyState.PENDING_EXIT, StrategyState.LONG
            ):
                # SELL fills also arrive when the bracket SL or TP closes
                # the position — treat either as "we're flat again."
                self._state = StrategyState.FLAT
                self._position_qty = None
        elif event.event in ("ORDER_REJECTED", "ORDER_CANCELLED", "ORDER_EXPIRED"):
            if self._state == StrategyState.PENDING_ENTRY:
                self._state = StrategyState.FLAT
                self._position_qty = None
            elif self._state == StrategyState.PENDING_EXIT:
                self._state = StrategyState.LONG

    async def run(self) -> None:
        log.info(
            "starting RSI momentum period=%d oversold=%s overbought=%s "
            "equity_fraction=%s (symbol learned from the stream)",
            self._cfg.period, self._cfg.oversold,
            self._cfg.overbought, self._cfg.equity_fraction,
        )
        async with self._gw.stream() as events:
            async for event in events:
                match event:
                    case CandleEvent():
                        await self.on_candle(event)
                    case OrderEvent():
                        self.on_order(event)
                    case ConnectionEvent(event=ev, broker=broker, error=err):
                        log.info("connection %s broker=%s error=%s", ev, broker, err)
                    case ErrorEvent(code=code, message=msg):
                        log.error("gateway error code=%s message=%s", code, msg)


# ---------------------------------------------------------------------------
# Entrypoint
# ---------------------------------------------------------------------------


def _install_signal_handlers(loop: asyncio.AbstractEventLoop, stop: asyncio.Event) -> None:
    """Flip `stop` on SIGTERM/SIGINT so Cloud Run's 10s grace shutdown is clean.

    Signal handlers are not available on Windows' asyncio loop; the fallback
    is a NotImplementedError, which we silently tolerate.
    """
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, stop.set)
        except NotImplementedError:
            return


async def main() -> None:
    logging.basicConfig(
        level=os.environ.get("LOG_LEVEL", "INFO").upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    cfg = Config.from_env()

    loop = asyncio.get_running_loop()
    stop = asyncio.Event()
    _install_signal_handlers(loop, stop)

    async with AsyncTradingGateway() as gw:
        strategy = RsiMomentumStrategy(gw, cfg)
        # Warm-up runs lazily on the first candle, once the subscribed symbol
        # is known — see RsiMomentumStrategy.on_candle.
        run_task = asyncio.create_task(strategy.run(), name="strategy")
        stop_task = asyncio.create_task(stop.wait(), name="stop")
        done, _ = await asyncio.wait(
            {run_task, stop_task}, return_when=asyncio.FIRST_COMPLETED
        )
        if stop_task in done:
            log.info("shutdown signal received")
            run_task.cancel()
            try:
                await run_task
            except asyncio.CancelledError:
                pass
        else:
            stop_task.cancel()
            await run_task  # re-raise any strategy exception


if __name__ == "__main__":
    asyncio.run(main())
