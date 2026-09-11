# AUR packages

`wizard-bin` installs the release binary (x86_64 and aarch64 gnu tarballs, sha256 from the release's `checksums.txt`). `wizard` builds the release tag with cargo. Neither is on the AUR yet; both names were free when this was written. `bump.sh VERSION checksums.txt` points both at a release, and the release workflow runs it and commits the result.

To publish once there's an AUR account: add an ssh key to the account on aur.archlinux.org, then for each package

    git clone ssh://aur@aur.archlinux.org/wizard-bin.git
    cp contrib/aur/wizard-bin/PKGBUILD contrib/aur/wizard-bin/.SRCINFO wizard-bin/
    cd wizard-bin && git add PKGBUILD .SRCINFO && git commit -m "Update to 3.0.1" && git push

The first push creates the package. `.SRCINFO` must match the PKGBUILD; regenerate it with `makepkg --printsrcinfo > .SRCINFO` after any hand edit, and check with `namcap PKGBUILD` and `makepkg -s`.
