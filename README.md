# fv-search

Rust GUI port of `../find.py`. Cross-platform, parallel file-content search.

## Run

```sh
cd search
cargo run --release
```

Binary: `target/release/fv-search` (Windows: `fv-search.exe`).
Cross build: same repo builds on Windows / macOS / Linux, no code change.

## Map from find.py

| find.py | GUI |
|---|---|
| `glob(root/**/*.py, recursive=True)` | Folder picker + `Recursive` checkbox (`ignore::WalkBuilder`, max_depth=1 khi tắt) |
| `filepath.endswith(".py")` | Extension checkboxes (24 preset common) + Custom input. Bỏ chọn hết = all files |
| `"node_module" in filepath …` | `Skip dirs` substring, comma-separated, default `node_module,venv,dist,build` |
| `"util"/"clean" not in filepath` | `Skip path` substring, default `util,clean` |
| `"arquivos" in content` | `Contains` input, empty = match all. `Case sensitive` toggle |
| `fs.sort(key=mtime)` | Sort dropdown, default `modified time ↑` |
| `print(count)` + list | Status bar `scanned/matched/ms` + results list, preview dòng match đầu tiên |

## Perf

- Walk: `ignore` crate (gitignore-aware, symlink loop-safe, skip binary NUL).
- Match: `rayon` parallel trên mọi CPU, channel stream kết quả live về egui.
- Guard: `Max file size (MB)` skip file lớn, render cap 5000 rows, binary skip.
- Release profile: `lto + strip + opt-level=3`, 1 codegen-unit.

## Use

1. `Browse…` chọn folder (default `D:\workspace\fv`).
2. Nhập `Contains`, tick ext, Enter hoặc `Search`.
3. Click result: xem preview, `Open file / Open folder / Copy path`.
4. `Stop` hủy giữa chừng, `Clear` xóa kết quả.
