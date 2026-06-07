"""Moving Average Crossover — Tektii platform reference strategy.

A trend-following strategy that opens a long position when a short-period
Simple Moving Average crosses above a long-period SMA ("golden cross") and
closes it on the reverse crossover ("death cross"). One position at a time;
the entry is a market order with optional bracket stop-loss and take-profit.

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
MA_SHORT             10              Short SMA period, in bars
MA_LONG              20              Long SMA period, in bars
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
entry and sold in full on the death-cross exit.

The helper returns a full-precision quantity. The mock provider used in
backtests accepts it as-is; real brokers (Alpaca, Binance, Oanda) require it
rounded to the instrument's lot size before submitting.

Local run
---------
    pip install -e .
    python strategy.py

Docker
------
    docker build -t tektii-template-ma-crossover:dev .
    docker run --rm --network=host tektii-template-ma-crossover:dev
"""

from __future__ import annotations

import asyncio
import logging
import os
import signal
from collections import deque
from dataclasses import dataclass
from decimal import Decimal
from enum import Enum, auto
from typing import Deque

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

log = logging.getLogger("ma_crossover")


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Config:
    equity_fraction: Decimal
    short_window: int
    long_window: int
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
                short_window=int(os.environ.get("MA_SHORT", "10")),
                long_window=int(os.environ.get("MA_LONG", "20")),
                timeframe=os.environ.get("TIMEFRAME", "1m"),
                stop_loss_pct=opt_decimal("STOP_LOSS_PCT"),
                take_profit_pct=opt_decimal("TAKE_PROFIT_PCT"),
            )
        except (ValueError, ArithmeticError) as err:
            raise SystemExit(f"Invalid configuration: {err}") from err
        if cfg.equity_fraction <= 0:
            raise SystemExit("ORDER_EQUITY_FRACTION must be positive")
        if cfg.short_window <= 0 or cfg.long_window <= 0:
            raise SystemExit("MA_SHORT and MA_LONG must be positive")
        if cfg.short_window >= cfg.long_window:
            raise SystemExit(
                f"MA_SHORT ({cfg.short_window}) must be < MA_LONG ({cfg.long_window})"
            )
        return cfg


# ---------------------------------------------------------------------------
# State machine
# ---------------------------------------------------------------------------


class StrategyState(Enum):
    """Single-position lifecycle. Transitions are driven by a successful
    order submit, not by fill events — see ``_enter_long`` / ``_exit_long``.
    """

    FLAT = auto()
    LONG = auto()


class Cross(Enum):
    """Prior-bar relationship between the short and long SMAs."""

    UNKNOWN = auto()
    ABOVE = auto()
    BELOW = auto()


# ---------------------------------------------------------------------------
# Indicator helpers (pure, unit-testable)
# ---------------------------------------------------------------------------


def compute_sma(values: Deque[Decimal]) -> Decimal:
    """Simple moving average across `values`. Caller ensures non-empty."""
    return sum(values, start=Decimal(0)) / Decimal(len(values))


def bracket_prices(
    entry: Decimal,
    stop_loss_pct: Decimal | None,
    take_profit_pct: Decimal | None,
) -> tuple[Decimal | None, Decimal | None]:
    """Compute SL/TP prices for a LONG entry from a reference price.

    The reference is the signal-bar close, which is a known simplification:
    the actual market-order fill happens at the next bar's open. For a
    tighter template, wait for the ORDER_FILLED event, read
    ``order.average_fill_price``, then submit the bracket legs against
    that. Keeping it simple here so the core flow stays readable.

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


class MaCrossoverStrategy:
    def __init__(self, gw: AsyncTradingGateway, cfg: Config) -> None:
        self._gw = gw
        self._cfg = cfg
        self._short: Deque[Decimal] = deque(maxlen=cfg.short_window)
        self._long: Deque[Decimal] = deque(maxlen=cfg.long_window)
        self._cross = Cross.UNKNOWN
        self._state = StrategyState.FLAT
        self._warmed_up = False
        # Quantity of the open position, set on entry and sold in full on exit.
        # None while flat.
        self._position_qty: Decimal | None = None

    async def warm_up(self, symbol: str) -> None:
        """Seed the SMA windows from historical bars for ``symbol``.

        Runs lazily on the first candle (see ``on_candle``): the strategy is
        symbol-agnostic, so it only learns which instrument to back-fill once
        the stream delivers the first bar.

        Without this, the strategy discards the first ``long_window`` live
        bars as warm-up. With it, the first live bar can already produce a
        cross signal.

        Best-effort: any SDK error (connection, auth, 5xx, bad timeframe)
        falls back to cold-start with a warning so a transient history
        outage doesn't kill the strategy on boot.
        """
        limit = self._cfg.long_window + WARMUP_MARGIN_BARS
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

        for bar in bars:
            close = Decimal(bar.close)
            self._short.append(close)
            self._long.append(close)
            if len(self._long) >= self._cfg.long_window:
                short_ma = compute_sma(self._short)
                long_ma = compute_sma(self._long)
                self._cross = Cross.ABOVE if short_ma > long_ma else Cross.BELOW

        log.info(
            "warmed up from history: %d bars, cross=%s",
            len(bars), self._cross.name,
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
        self._short.append(close)
        self._long.append(close)

        # Warm-up: need a full long window before we can compare SMAs.
        if len(self._long) < self._cfg.long_window:
            return

        short_ma = compute_sma(self._short)
        long_ma = compute_sma(self._long)
        # Equality counts as BELOW so an exact cross at `==` does not flap.
        new_cross = Cross.ABOVE if short_ma > long_ma else Cross.BELOW

        log.debug(
            "bar close=%s short=%s long=%s cross=%s state=%s",
            close, short_ma, long_ma, new_cross.name, self._state.name,
        )

        prev = self._cross
        self._cross = new_cross

        if prev == Cross.BELOW and new_cross == Cross.ABOVE and self._state == StrategyState.FLAT:
            await self._enter_long(symbol=bar.symbol, entry_ref=close)
        elif prev == Cross.ABOVE and new_cross == Cross.BELOW and self._state == StrategyState.LONG:
            await self._exit_long(symbol=bar.symbol)

    async def _enter_long(self, *, symbol: str, entry_ref: Decimal) -> None:
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
        log.info(
            "golden cross on %s: submitting BUY qty=%s sl=%s tp=%s",
            symbol, quantity, sl, tp,
        )
        try:
            handle = await self._gw.submit_order(
                symbol=symbol,
                side="buy",
                quantity=quantity,
                stop_loss=sl,
                take_profit=tp,
            )
        except OrderRejectedError as err:
            log.warning("order rejected code=%s message=%s", err.code, err.message)
            return  # rejected: stay FLAT
        # Submit accepted — go LONG without waiting for a fill, and record the
        # quantity so the exit can sell the whole position.
        self._state = StrategyState.LONG
        self._position_qty = quantity
        log.info("order accepted id=%s status=%s -> LONG", handle.id, handle.status)

    async def _exit_long(self, *, symbol: str) -> None:
        quantity = self._position_qty
        # A LONG position always has a recorded quantity; guard against selling
        # an unknown size.
        if quantity is None:
            log.error("exit requested on %s with no tracked quantity; staying LONG", symbol)
            return
        log.info(
            "death cross on %s: submitting SELL qty=%s",
            symbol, quantity,
        )
        try:
            handle = await self._gw.submit_order(
                symbol=symbol,
                side="sell",
                quantity=quantity,
            )
        except OrderRejectedError as err:
            log.warning("order rejected code=%s message=%s", err.code, err.message)
            return  # rejected: stay LONG, keep the remembered quantity
        # Submit accepted — go FLAT.
        self._state = StrategyState.FLAT
        self._position_qty = None
        log.info("order accepted id=%s status=%s -> FLAT", handle.id, handle.status)

    def on_order(self, event: OrderEvent) -> None:
        """Reconcile live order events. Not invoked during a backtest."""
        order = event.order
        log.info(
            "order %s id=%s symbol=%s side=%s qty=%s filled=%s status=%s",
            event.event, order.id, order.symbol, order.side,
            order.quantity, order.filled_quantity, order.status,
        )
        # A SELL fill while LONG means the position closed (e.g. a bracket
        # SL/TP) — go FLAT so the next golden cross can re-enter.
        if (
            event.event == "ORDER_FILLED"
            and order.side == "SELL"
            and self._state == StrategyState.LONG
        ):
            log.info("position closed by fill (likely bracket SL/TP) -> FLAT")
            self._state = StrategyState.FLAT
            self._position_qty = None

    async def run(self) -> None:
        log.info(
            "starting MA crossover short=%d long=%d equity_fraction=%s "
            "sl_pct=%s tp_pct=%s (symbol learned from the stream)",
            self._cfg.short_window, self._cfg.long_window,
            self._cfg.equity_fraction, self._cfg.stop_loss_pct, self._cfg.take_profit_pct,
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
        strategy = MaCrossoverStrategy(gw, cfg)
        # Warm-up runs lazily on the first candle, once the subscribed symbol
        # is known — see MaCrossoverStrategy.on_candle.
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
