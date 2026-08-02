# tektii-gateway-mock

Mock provider adapter for Trading Gateway — generates synthetic market data for local strategy development and testing without broker credentials.

## Simulating partial fills

By default every order fills in a single tranche. Set `MOCK_PARTIAL_FILL_RATIO` to a
value between 0 and 1 (exclusive) to make orders fill in two tranches instead, so a
strategy can exercise its partial-fill handling locally:

```bash
GATEWAY_PROVIDER=mock MOCK_PARTIAL_FILL_RATIO=0.4 cargo run
```

With `0.4`, an order for 10 units fills 4 first — status `partially_filled`, emitting an
`OrderPartiallyFilled` event — then the remaining 6 half a second later, emitting
`OrderFilled` with the quantity-weighted average price. Each tranche books its own trade
and moves the position. Cancelling in between stops the remainder, leaving the order
cancelled with only the partial quantity filled. Values outside the range are ignored
with a warning.
