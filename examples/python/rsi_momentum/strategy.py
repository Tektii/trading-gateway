"""RSI Mean Reversion — Tektii platform reference strategy.

A mean-reversion strategy that opens a long position when the Relative
Strength Index enters the oversold zone (by default RSI < 30) and closes it
when RSI enters the overbought zone (by default RSI > 70). Signals fire on
*zone entry*, not while in-zone, so the strategy doesn't over-trade.

Designed to be the canonical shape for strategies on the Tektii platform:
stateful class, pure indicator helpers, SDK-native event dispatch, and a
small state machine that survives partial fills and broker rejects.

Environment variables
---------------------
===================  ==============  ==============================================
Name                 Default         Description
===================  ==============  ==============================================
SYMBOL               EUR/USD         Trading symbol
ORDER_QUANTITY       0.01            Order size (fractional allowed on forex/crypto)
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

Local run
---------
    pip install -e .
    SYMBOL=EUR/USD python strategy.py

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
    symbol: str
    quantity: Decimal
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
                symbol=os.environ.get("SYMBOL", "EUR/USD"),
                quantity=Decimal(os.environ.get("ORDER_QUANTITY", "0.01")),
                period=int(os.environ.get("RSI_PERIOD", "14")),
                oversold=Decimal(os.environ.get("RSI_OVERSOLD", "30")),
                overbought=Decimal(os.environ.get("RSI_OVERBOUGHT", "70")),
                timeframe=os.environ.get("TIMEFRAME", "1m"),
                stop_loss_pct=opt_decimal("STOP_LOSS_PCT"),
                take_profit_pct=opt_decimal("TAKE_PROFIT_PCT"),
            )
        except (ValueError, ArithmeticError) as err:
            raise SystemExit(f"Invalid configuration: {err}") from err
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

    async def warm_up(self) -> None:
        """Seed the Wilder RSI state from historical bars.

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
                self._cfg.symbol, self._cfg.timeframe, limit=limit
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
        if bar.symbol != self._cfg.symbol:
            return

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
            await self._enter_long(entry_ref=close, rsi=rsi)
        elif (
            new_zone == Zone.OVERBOUGHT
            and prev != Zone.OVERBOUGHT
            and self._state == StrategyState.LONG
        ):
            await self._exit_long(rsi=rsi)

    async def _enter_long(self, *, entry_ref: Decimal, rsi: Decimal) -> None:
        sl, tp = bracket_prices(
            entry_ref, self._cfg.stop_loss_pct, self._cfg.take_profit_pct
        )
        self._state = StrategyState.PENDING_ENTRY
        log.info(
            "oversold on %s rsi=%s: submitting BUY qty=%s sl=%s tp=%s",
            self._cfg.symbol, rsi, self._cfg.quantity, sl, tp,
        )
        try:
            handle = await self._gw.submit_order(
                symbol=self._cfg.symbol,
                side="buy",
                quantity=self._cfg.quantity,
                stop_loss=sl,
                take_profit=tp,
            )
            log.info("order accepted id=%s status=%s", handle.id, handle.status)
        except OrderRejectedError as err:
            log.warning("order rejected code=%s message=%s", err.code, err.message)
            self._state = StrategyState.FLAT

    async def _exit_long(self, *, rsi: Decimal) -> None:
        self._state = StrategyState.PENDING_EXIT
        log.info(
            "overbought on %s rsi=%s: submitting SELL qty=%s",
            self._cfg.symbol, rsi, self._cfg.quantity,
        )
        try:
            handle = await self._gw.submit_order(
                symbol=self._cfg.symbol,
                side="sell",
                quantity=self._cfg.quantity,
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
        if order.symbol != self._cfg.symbol:
            return
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
        elif event.event in ("ORDER_REJECTED", "ORDER_CANCELLED", "ORDER_EXPIRED"):
            if self._state == StrategyState.PENDING_ENTRY:
                self._state = StrategyState.FLAT
            elif self._state == StrategyState.PENDING_EXIT:
                self._state = StrategyState.LONG

    async def run(self) -> None:
        log.info(
            "starting RSI momentum symbol=%s period=%d oversold=%s overbought=%s qty=%s",
            self._cfg.symbol, self._cfg.period, self._cfg.oversold,
            self._cfg.overbought, self._cfg.quantity,
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
        await strategy.warm_up()
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
