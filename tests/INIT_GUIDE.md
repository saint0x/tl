# Fozzy Init Guide

This scaffold is set up to run with strict mode by default.
Use `--unsafe` only when intentionally opting out of strict checks.

## Recommended first run
```bash
fozzy full --scenario-root tests --seed 7
```

## Targeted commands
- Run deterministic scenarios: `fozzy test tests/*.fozzy.json --det --json`
- Run memory checks: `fozzy run tests/memory.pass.fozzy.json --det --mem-track --fail-on-leak --leak-budget 0 --json`
- Run distributed explore: `fozzy explore tests/distributed.pass.fozzy.json --schedule coverage_guided --nodes 3 --steps 200 --json`
- Run fuzzing: `fozzy fuzz scenario:tests/run.pass.fozzy.json --mode coverage --time 10s --record /tmp/timelapse-fuzz.fozzy --json`
- Run host-backed checks: `fozzy run tests/host.pass.fozzy.json --det --proc-backend host --fs-backend host --http-backend host --json`

Edit the `tests/*.fozzy.json` scenarios with your own inputs and assertions.
