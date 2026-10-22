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

## Duplicates

```sh
shoebox dupes --dry-run                         # exact copies, whole library, report only
shoebox dupes --album "Trip 2024"               # exact copies of anything in the album
shoebox dupes --match near --from 2019-01-01 --to 2019-06-30
shoebox dupes --match similar --threshold 12    # looser still
```

Nothing is deleted. Exact copies (same original file) keep one survivor, the copy
with the most on it, with the others' albums, keywords, favorite and title merged
onto it. The rest go to `Shoebox/Shoebox Delete <week>`, which you select-all and
delete in Photos. Look-alikes (`near`, `similar`) go as whole groups to
`Shoebox/Shoebox Review <week> #n` albums of about 32. Delete what you don't
want, take the keepers out, and the next run removes the emptied albums from
earlier weeks. Dates don't matter (the survivor keeps its own) and a missing
location is filled in from a copy. Copies with different edits or conflicting
locations are skipped and counted rather than merged.

It reads `Photos.sqlite` read-only (`--library` for a non-default library), so
the terminal needs access to the library package. Grant it Full Disk Access if
reads fail.

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
