# UI spikes

Throwaway programs that show 1,000 process rows updated in place every second.
Each is its own Cargo workspace, so the main build and CI never need GTK or Slint.

```
cd spikes/gtk-table   && cargo run --release   # needs libgtk-4-dev
cd spikes/slint-table && cargo run --release
```

## Results (Xvfb in a 4-core cloud container, 20 s run, 1,000 rows rewritten each second)

| | GTK4 (gtk4-rs 0.9, cairo renderer) | Slint 1.x (software renderer, winit) |
|---|---|---|
| RSS | 107 MiB | 18 MiB |
| CPU while updating | ~1% of one core | ~0.4% of one core |
| Release binary | 0.7 MB (links system GTK) | 24 MB (statically linked) |
| Sorting | Built in (`ColumnView` + `CustomSorter`), selection survives in-place updates | `StandardTableView` has sort callbacks, but sorting and selection tracking are ours to write |
| Native look | Yes, follows the desktop theme | No, its own styling |
| License | LGPL | GPLv3, royalty-free, or commercial. Check this against the repo license |

Caveats: Xvfb with cairo is not a real desktop. GTK's RSS counts the shared GTK/GLib/Pango libraries, which a GNOME session already has loaded, so the real extra cost there is lower. Rendering cost on real hardware was not measured.

## Takeaway

Slint is clearly lighter on RAM and CPU. GTK4 is more native and gives sorting and selection for free, and its table handled in-place updates fine with very little boilerplate (`BoxedAnyObject` plus `items_changed`). The call is "native feel and less code" (GTK4) vs "smallest footprint" (Slint).
