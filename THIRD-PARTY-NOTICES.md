# Third-party notices

Preserve the following notices alongside the corresponding assets:

| Material | Notice / provenance |
| --- | --- |
| Original Smith UI markup, icons and fonts | `vendor/smith-ui/SOURCE.json`; application license remains unselected |
| Inter font | `web/assets/fonts/licenses/Inter-OFL.txt`; original font notice also retained under `vendor/smith-ui/assets/fonts/` |
| Poppins font | `web/assets/fonts/licenses/Poppins-OFL.txt` |
| Spline Sans Mono font | `web/assets/fonts/licenses/SplineSansMono-OFL.txt` |
| Noto Sans document font | `crates/smith-media/assets/fonts/OFL.txt` and `PROVENANCE.md` |
| Swift/MLX dependencies | `desktop/mlx/ThirdPartyNotices/` and pinned `desktop/mlx/Package.resolved` |
| Rust/Slint dependencies | Pinned `Cargo.lock` files and the original packages' license files and metadata |

The original UI font directory's single OFL notice is not a substitute for the
separate Poppins and Spline Sans Mono notices. Font bytes and available notices
are retained without redrawing, renaming or claiming authorship.

Rust and Swift source dependencies are resolved through their package managers;
their full implementation trees are not vendored here. A future binary package
must carry its applicable notices and satisfy its dependency and application
license requirements. A dependency inventory is not a declaration of blanket
redistribution permission.
