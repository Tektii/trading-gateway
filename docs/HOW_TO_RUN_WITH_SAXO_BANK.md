# Saxo Bank Credentials Setup

How to get the credentials needed to run the Trading Gateway with the Saxo Bank adapter.

## How Saxo Authentication Works

Saxo Bank uses OAuth2 with a multi-layered identity model. Unlike brokers like Alpaca (API key + secret) or Oanda (single bearer token), Saxo separates the _application_ from the _user_ from the _account_:

- **Application** (app key + secret) — identifies your registered app. Created once in the developer portal.
- **User** (access token + refresh token) — identifies the logged-in user. Obtained via OAuth2 authorization code flow, or as a 24-hour token from the developer portal for testing.
- **Account** (account key) — identifies which trading sub-account to operate on. A single Saxo client can have multiple accounts (e.g. separate FX and equities accounts), so every order and position request requires an explicit account key.

Saxo also runs two completely separate environments — **SIM** (simulation/paper) and **LIVE** — with different URLs and credentials. The gateway defaults to SIM; set `GATEWAY_MODE=live` for production.

## 1. Create a Saxo Developer Account

Sign up at [developer.saxo](https://www.developer.saxo/) and log in. This gives you access to the SIM (demo) environment.

## 2. Create an Application

1. Go to **My Applications** in the developer portal
2. Create a new application
3. Note down:
   - **Application Key** — this is your `SAXO_APP_KEY`
   - **Application Secret** — this is your `SAXO_APP_SECRET`

## 3. Get Your Account Key

Your Account Key identifies which sub-account to trade on. Get it by calling the Saxo API with an access token (you can generate a 24-hour token from the developer portal):

```bash
curl -s -H "Authorization: Bearer YOUR_ACCESS_TOKEN" \
  https://gateway.saxobank.com/sim/openapi/port/v1/accounts/me \
  | python3 -m json.tool
```

The `AccountKey` field in the response is your `SAXO_ACCOUNT_KEY`.

Alternatively, you can use the client endpoint which returns `DefaultAccountKey`:

```bash
curl -s -H "Authorization: Bearer YOUR_ACCESS_TOKEN" \
  https://gateway.saxobank.com/sim/openapi/port/v1/clients/me \
  | python3 -m json.tool
```

## 4. (Optional) Get an Access Token

The developer portal provides a 24-hour access token for testing. Set it as `SAXO_ACCESS_TOKEN` to skip the OAuth flow at startup.

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `SAXO_APP_KEY` | Yes | Application key from the developer portal |
| `SAXO_APP_SECRET` | Yes | Application secret from the developer portal |
| `SAXO_ACCOUNT_KEY` | Yes | Account key from the `/port/v1/accounts/me` endpoint |
| `SAXO_ACCESS_TOKEN` | No | Pre-seeded access token (24h from dev portal) |
| `SAXO_REFRESH_TOKEN` | No | Refresh token for automatic token renewal |

## Running the Gateway

### Docker

```bash
# Build
docker build -f docker/Dockerfile -t tektii/gateway:latest .

# Run (paper/SIM mode is the default)
docker run -d --name gateway \
  -e GATEWAY_PROVIDER=saxo \
  -e SAXO_APP_KEY=your_app_key \
  -e SAXO_APP_SECRET=your_app_secret \
  -e SAXO_ACCOUNT_KEY='your_account_key' \
  -e SAXO_ACCESS_TOKEN=your_access_token \
  -e GATEWAY_HOST=0.0.0.0 \
  -p 8080:8080 \
  tektii/gateway:latest

# Wait for ready
until curl -sf http://localhost:8080/readyz; do sleep 1; done

# Verify
curl http://localhost:8080/v1/account
```

### Local Binary

```bash
GATEWAY_PROVIDER=saxo \
SAXO_APP_KEY=your_app_key \
SAXO_APP_SECRET=your_app_secret \
SAXO_ACCOUNT_KEY='your_account_key' \
SAXO_ACCESS_TOKEN=your_access_token \
cargo run --release --bin tektii-gateway
```

Note: quote the `SAXO_ACCOUNT_KEY` value — Saxo account keys often contain pipe (`|`) and equals (`=`) characters that need shell escaping.
