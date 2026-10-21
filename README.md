# shoebox

Script Apple Photos from the shell. A Rust port of
[PhotoScript](https://github.com/RhetTbull/PhotoScript): same AppleScript
handlers (embedded verbatim), called in-process through `NSAppleScript`, no
Python.

```sh
make install                       # -> ~/.local/bin/shoebox

shoebox info
shoebox photos --search beach | head -5 | xargs shoebox show | jq .filename
shoebox import ~/Desktop/*.heic --album "Inbox" --skip-duplicate-check
shoebox set <uuid> --title "Sunset" --keywords "trip,2026" --favorite true
shoebox export <uuid> --to ~/Desktop/out --original
shoebox album create "Highlights" --folder "Trips/2026"
shoebox album export "Highlights" --to ~/Desktop/highlights
shoebox albums | jq -r .path
```

Lists print one uuid per line; details print JSON lines. `shoebox help <cmd>`
for everything else.

The first run triggers macOS's Automation prompt ("… wants to control
Photos") for your terminal; allow it.

## Library

```rust
let lib = shoebox::Photos::new()?;
let album = lib.make_album_folders("Highlights", &["Trips".into(), "2026".into()])?;
let photo = lib.photo("EC19EA1A-FC91-449C-8925-B13D863E2EDB")?;
photo.set_keywords(&["beach".into()])?;
album.add(&[photo])?;
```

## Caveats (inherited from Photos' AppleScript)

- Each property read is a round trip to Photos. Photos serves several clients
  at once, so multi-photo `show`, `set`, `export` and `album export` split
  across `-j` worker processes (default 4); output stays in input order.
- `album remove` and `album move` recreate the album (its uuid changes).
- Only top-level folders can be deleted.
- Importing a duplicate without `--skip-duplicate-check` blocks on a dialog.
