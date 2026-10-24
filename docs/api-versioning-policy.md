# API and SDK versioning policy

## REST

Major versions are identified by URL path (`/api/v1/`, `/api/v2/`). Clients may
also send `X-API-Version: 1` or an
`Accept: application/vnd.zenith.v1+json` media type. When both a URL version
and a version header are present, they must agree. Unsupported or conflicting
versions receive `406` or `400`, respectively. Responses include
`API-Version` when a version was selected.

Changes within a major version are additive. Removing or changing the meaning
of a field, route, or error contract requires a new major version. Deprecated
routes are configured with comma-separated exact paths in
`ZENITH_DEPRECATED_ROUTES`, plus `ZENITH_DEPRECATION_TIMESTAMP` (Unix seconds)
and `ZENITH_SUNSET_DATE` (HTTP-date). Responses include `Deprecation`,
`Sunset`, and a `Link` to this policy document. Operators should provide at
least twelve months between announcing deprecation and sunset.

## WebSockets and SDKs

WebSocket clients can negotiate the `zenith.v1` or `zenith.v2` subprotocol;
the handshake response selects the negotiated subprotocol and reports
`API-Version`. SDK packages are released with the OpenAPI major version and
must not silently switch a client's major version. Generated client
deprecations follow the REST route sunset schedule.

The OpenAPI source files (`openapi/openapi.yaml` and `openapi.json`) must both
contain every settlement/privacy/keeper route registered in the router. CI
runs `scripts/check-settlement-openapi.mjs` to enforce that route/spec parity.
