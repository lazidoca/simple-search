# fv-search

Cross-platform GUI for fast parallel file-content search. Pick a folder,
type a keyword, get a live result list. Single binary, no runtime needed.

## Run

```sh
cd search
cargo run --release
```

Binary: `target/release/fv-search` (Windows: `fv-search.exe`).
Cross build: same repo builds on Windows / macOS / Linux, no code change.

## Features

- Folder picker + `Recursive` toggle for nested folders
- Extension filter: 24 common presets + custom input (empty = all files)
- Content search with `Case sensitive` toggle (empty query = list all files)
- Skip dirs / skip path by substring (`node_module,venv,dist,build` …)
- Sort by modified time / path / size
- Click any result to open it; right-click for folder / copy path
- Remembers last folder + search term + filters in `prefs.json` next to binary

## Perf

- Walk: parallel `ignore` walker, excluded dirs pruned (never descended),
  gitignore-aware, symlink loop-safe.
- Match: `rayon` across all CPUs, byte-level `memchr` scan, only the single
  preview line is UTF-8 converted. Binary skip via NUL sniff.
- Guards: `Max file size (MB)` skips big files, render cap 5000 rows.
- Header shows `visited • read • matched • total ms (walk + match)` so the
  bottleneck is always visible.
- Release profile: `lto + strip + opt-level=3`, 1 codegen-unit.
- Always benchmark with `--release`: debug builds are 5–20× slower.

## Use

1. `Browse…` chọn folder.
2. Nhập `Contains`, tick ext, Enter hoặc `Search`.
3. Click result để open file. Chuột phải: open folder / copy path.
4. `Stop` hủy giữa chừng, `Clear` xóa kết quả.
