# winwincode

ESM host and command-line entry for WinWinCode. The package supports macOS and GNU Linux on arm64 and x64.

## Commands

```bash
winwincode                         # start DSH Web with Chat selected
winwincode web --no-open --port 0  # pass options to the DSH Web host
winwincode delivery help           # list the Delivery operations
winwincode --print-scaffold        # print the installed surface descriptor
```

The launcher creates the `winwincode` DSH profile with the stock DSH base and Web layers, then applies `@winwincode/dsh-profile` as the final layer. Chat remains the default view and StrongFlow remains the advanced view.

DSH and the Delivery CLI share `$DSH_HOME/winwincode` as their durable home. Set `WINWINCODE_CLI_AUTH_PROOF` to the local peer proof used for a business Attention decision; the proof is checked in memory and is not written to the Delivery record.

Delivery commands emit one versioned JSON response. Their process exit codes are:

| Code | Meaning |
| ---: | --- |
| `0` | Request completed |
| `2` | Invalid command or request |
| `3` | Delivery not found |
| `4` | Stale revision, state conflict, or unresolved Attention |
| `5` | Local service failure |
| `130` | Interrupted by `SIGINT` |
| `143` | Interrupted by `SIGTERM` |

The installed-package gate is `corepack pnpm verify:installed-host`. It packs the release packages, builds a portable locked installation, starts the real DSH Web process twice, runs keyless Chat and StrongFlow roles, exercises Delivery creation/review/restart through separate CLI processes, checks signal and exit behavior, and removes its test home when it finishes.
