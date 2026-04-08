# gwatch

Watch command output with a graph.

`gwatch` is a terminal UI tool for ad-hoc metric monitoring. It runs shell commands on an interval, reads the first line of `stdout`, parses it as a number, and draws real-time time-series graphs.

It is based on the architecture and rendering approach from `gping`, but focuses on arbitrary numeric command output.

## Features

- Multiple independent watches running in parallel
- Per-watch interval (`period`) with global default
- Two config styles:
  - TOML file (`--file`)
  - CLI mini-DSL (`--watch`)
- Value types:
  - `float`
  - `bytes` (base 1024, formatted as `KiB/MiB/GiB`)
- Dual Y axis support:
  - left axis for `bytes`
  - right axis for `float`
  - independent scales for mixed-dimension lines
- Live graph rendering in terminal (ratatui/crossterm)
- Missing/invalid values are rendered as gaps

## Build and Run

```bash
cargo run -p gwatch -- --help
```

Install binary locally:

```bash
cargo install --path ./gwatch
```

## Usage

### 1) TOML config

```bash
gwatch -f config.toml
gwatch -f -
```

Example `config.toml`:

```toml
period = 1

[[watch]]
title = "Memory"
exp = "ps aux | grep oxide | grep -v grep | awk '{print $6}'"
measure = "bytes"
axis = "left"

[[watch]]
title = "CPU"
exp = "ps aux | grep oxide | grep -v grep | awk '{print $3}'"
period = 1
measure = "float"
axis = "right"
```

### 2) CLI mini-DSL

```bash
gwatch \
  --watch 'title=Memory;measure=bytes;axis=left;period=2.5;exp=ps aux | grep oxide | grep -v grep | awk "{print $6}"' \
  --watch 'title=CPU;measure=float;axis=right;period=1;exp=ps aux | grep oxide | grep -v grep | awk "{print $3}"'
```

DSL format:

```text
key=value;key=value;...
```

Required field: `exp`.
Optional field: `axis` (`left` or `right`).

## Runtime Rules

- Command is executed through shell:
  - Unix: `sh -c`
  - Windows: `cmd /C`
- First line of `stdout` is parsed as `float`
- If command fails, stdout is empty, or value is not numeric, the point is skipped
- Watches run independently, so one slow/blocked command does not block others

## CLI Reference

```bash
gwatch --help
```

Current core options:

- `-f, --file <FILE>`
- `--watch <WATCH_SPECS>`
- `-p, --period <SECONDS>`
- `-b, --buffer <SECONDS>`
- `-s, --simple-graphics`
- `--vertical-margin <N>`
- `--horizontal-margin <N>`
- `-c, --color <LIST>`
- `--clear`

## Controls

- `q` or `Esc` to exit
- `Ctrl+C` to exit
