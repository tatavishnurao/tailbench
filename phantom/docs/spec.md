# Script and token schedule

The script phantom follows, fixed at startup. The flags that set it are in [endpoints.md](endpoints.md#usage).

## Script

- **t0** is the arrival of the first chat or completions request, on the server's monotonic clock. All logged times are ns since t0.
- **seq** comes from an atomic counter at arrival: 0, 1, 2, ... with no gaps. Every seq gets exactly one log line, completed or abandoned (see [Abandoned requests](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/truth-log.md#abandoned-requests)).
- **scripted_service_ns** is a pure function of (seed, seq): splitmix64 feeds Box-Muller for lognormal; fixed is `ms * 1e6`.
- **stall_wait_ns** is `window_end - arrival_ns` if arrival falls in a stall window (start inclusive, end exclusive), else 0.
- A non-streaming request is held until `arrival + stall_wait_ns`, then sleeps `scripted_service_ns`, then gets its response. Streams follow the token schedule in [Token schedule](https://github.com/tatavishnurao/tailbench/blob/main/phantom/docs/spec.md#token-schedule) below.

## Token schedule

Timing, on the server's own clock:

- The schedule is absolute. Token 0 is scheduled at `arrival + scripted_ttft_ns`, and token k at token k−1's *scheduled* time + `scripted_itl_ns` for that gap, never at its actual hand-off. phantom sleeps until each scheduled time, so timer overshoot on one token does not delay the next: it does not accumulate over a stream. Each token's scheduled time is logged in `scheduled_token_ns`, and `token_ns[k] >= scheduled_token_ns[k]` for every k.
- If a token's scheduled time falls inside a stall window, that token and every later scheduled time move later by `window end − scheduled time` (logged in `token_stall_wait_ns` for that token, 0 for the others). The gaps scripted after the stall are kept as they are. A token moved to a window's end is not moved again, even if another window starts there: each window is applied separately, as for non-streaming requests.
- The chat role chunk is sent according to `--role-chunk`. With `with-first-token` (default), phantom hands it off just before token 0, so a client that times the first chunk and one that times the first content chunk see the same TTFT. With `at-arrival`, phantom hands it off as soon as the request arrives. That is before TTFT, and also before any stall, so the two clients see different TTFTs. Either way, chat lines log the hand-off time in `role_chunk_ns`.
- `scripted_ttft_ns` is a pure function of (seed, seq), and each `scripted_itl_ns` of (seed, seq, token index). Both use splitmix64 and Box-Muller like the service time, with their own keys, so they never reuse the service-time draw or each other's. With no `--ttft-*`, TTFT follows the service-time spec but takes its own draws.
