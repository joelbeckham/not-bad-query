# Application icon

The database/bolt glyph is Lucide's `database-zap`, adapted onto a dark green tile
with mint database lines and an amber bolt. Source:
https://lucide.dev/icons/database-zap

`database-zap.svg` and `LICENSE-lucide` were downloaded from Lucide revision
`92f3fb0f905b31acccf139c68150f2724efbbf08`. The ISC license and upstream notices
are retained in `LICENSE-lucide`. `app.svg` is the application adaptation.

Regenerate the committed PNG and ICO files with:

```powershell
cargo run -p nbq-app --example generate_icon
```

The ICO includes 16, 20, 24, 32, 40, 48, 64, 128, and 256 pixel frames for Windows
taskbar/Explorer DPI scaling. The title bar uses the same design from embedded PNG
bytes, so no installation-relative asset paths are required at runtime.
The Windows executable also embeds the upstream license as resource 2.
