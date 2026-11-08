# ml2165-rust

> [!NOTE]
> **Status: complete.** The driver does what it set out to do and is verified on
> the hardware it targets. The 106 unit tests pass, and the PPD's page geometry
> was checked physically: an A4 test page with a 100 mm calibration bar and a
> frame 20 mm from each paper edge was printed and measured with a ruler, and
> the margins came out on size. No further development is planned — bug reports
> and pull requests are still welcome.

A CUPS raster filter (`rastertospl-rust`) for the Samsung ML-2165 monochrome laser printer, written in Rust. It converts CUPS's standard raster stream (`RaSt`/`RaS2`/`RaS3`) into the printer's native binary **SPL2 / QPDL v3** format: PJL job envelope, 17-byte page header, Algo 0x11 RLE-compressed band records, and checksums.

The protocol implementation was verified against the actual source of the [OpenPrinting SpliX](https://github.com/OpenPrinting/splix) driver (`document.cpp`, `compress.cpp`, `qpdl.cpp`, `algo0x11.cpp`, `printer.cpp`) and tested on real hardware.

## Supported Models

ML-2165, attached over USB. Network and wireless printers are not supported:
nothing here is written or tested for them, and the PPD advertises no other
model.

## Requirements

- Rust toolchain (`cargo`), 1.97.1 or newer — `Cargo.toml` declares it as
  `rust-version`, so an older `cargo` refuses to build. The installed package
  itself still runs on Debian 13; only building it there is unsupported.
- CUPS (`lpadmin`, `lpinfo`, `cupstestppd`)
- `ar`, `tar`, `xz` and `gzip` to build the package — `dpkg-dev` is not needed
- Printer powered on and attached over USB

## Installation

The Debian package is the supported way to install this driver. It puts the
filter and the PPD where CUPS expects them, registers the PPD in CUPS' own
driver database so the graphical tools can offer it by model name, and comes
back out with `apt remove`. The target platforms are Debian 13 (trixie) and
Debian 14. On anything else — or when working on the filter itself — use
[Manual installation](#manual-installation) further down instead.

There is no install script: the steps below *are* the procedure. Run them from
the repository root as your normal user. Only the `sudo` lines write to system
paths, so **don't run the whole sequence as root**.

### 1. Check that the repository path is under your control

```sh
namei -l .
```

Every component from `/` down to the repository must be owned by you or by
root, and none of them may be group- or world-writable (a sticky directory such
as `/tmp` is acceptable for the parents, since only an entry's owner can replace
it there). Check the build inputs too — `src/`, `Cargo.toml` and `Cargo.lock`
end up compiled into the binary, and `ppd/` into the installed PPD:

```sh
find . -path ./target -prune -o -perm /022 -print
```

This is not boilerplate caution. The package built in step 2 embeds the filter
binary, and step 3 installs it **as root**. A PPD is not a passive config file
either: its `*cupsFilter`/`*cupsFilter2` line names the program CUPS executes
for every print job, as user `lp`. Anyone who can write into the repository — or
replace any directory above it — can therefore have a program of their own
installed as a root-owned CUPS filter.

### 2. Build the package

The filter is an ordinary **glibc** build — it uses no third-party crates, so
the only things it links against are the C runtime and `libgcc_s`. That is what
the declared dependencies say: `cups`, `cups-filters`, `libc6 (>= 2.34)` and
`libgcc-s1 (>= 4.2)`. Both Debian releases ship a glibc newer than that, so a
package built on Debian 13 installs unchanged on Debian 14.

A `.deb` is an `ar` archive of three members, and `ar`, `tar`, `xz` and `gzip`
are enough to produce one. The control metadata lives in `packaging/debian/`
and its `Version:` field is what the file name below has to match.

```sh
cargo build --release

REPO=$PWD
VERSION=1.1.0-1
DOC=usr/share/doc/ml2165-rust
BUILD=$(mktemp -d)
mkdir -p "$BUILD"/root/usr/lib/cups/filter \
         "$BUILD"/root/usr/share/ppd/ml2165-rust \
         "$BUILD"/root/"$DOC" "$BUILD"/control

install -m 755 target/release/rastertospl-rust \
    "$BUILD/root/usr/lib/cups/filter/rastertospl-rust"
install -m 644 ppd/samsung-ml2165.ppd \
    "$BUILD/root/usr/share/ppd/ml2165-rust/samsung-ml2165.ppd"
install -m 644 packaging/debian/copyright "$BUILD/root/$DOC/copyright"
gzip -9nc packaging/debian/changelog > "$BUILD/root/$DOC/changelog.Debian.gz"
gzip -9nc README.md > "$BUILD/root/$DOC/README.md.gz"
chmod 644 "$BUILD"/root/"$DOC"/*.gz

awk -v s="$(du -sk --apparent-size "$BUILD/root" | cut -f1)" \
    '/^Description:/ && !d { print "Installed-Size: " s; d = 1 } { print }' \
    packaging/debian/control > "$BUILD/control/control"
( cd "$BUILD/root" && find . -type f -printf '%P\n' | LC_ALL=C sort | xargs md5sum ) \
    > "$BUILD/control/md5sums"
chmod 644 "$BUILD/control/control" "$BUILD/control/md5sums"

printf '2.0\n' > "$BUILD/debian-binary"
TARFLAGS="--owner=root --group=root --numeric-owner --sort=name --mtime=@0 --mode=go-w"
tar $TARFLAGS -C "$BUILD/root"    -cf - . | xz -9e > "$BUILD/data.tar.xz"
tar $TARFLAGS -C "$BUILD/control" -cf - . | xz -9e > "$BUILD/control.tar.xz"

mkdir -p "$REPO/dist"
( cd "$BUILD" && ar rcD "$REPO/dist/ml2165-rust_${VERSION}_amd64.deb" \
    debian-binary control.tar.xz data.tar.xz )
```

The `--owner=root --group=root --numeric-owner` flags are not cosmetic: they are
what makes the installed filter root-owned regardless of who built the package.
`--mode=go-w` is there for the same reason — without it the directories inherit
the build user's umask, and a `.deb` built under a 002 umask would install
group-writable directories under `/usr/lib/cups/filter` and `/usr/share/ppd`.

Check the result before installing it:

```sh
dpkg-deb -I dist/ml2165-rust_1.1.0-1_amd64.deb    # control metadata
dpkg-deb -c dist/ml2165-rust_1.1.0-1_amd64.deb    # paths, owners, modes
```

Everything in the listing must be `root/root`, the filter `755` and the data
files `644`.

If you extend the filter, re-derive the two library versions in
`packaging/debian/control` from the binary you are about to ship rather than
trusting the numbers there:

```sh
objdump -T target/release/rastertospl-rust | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1
objdump -T target/release/rastertospl-rust | grep -o 'GCC_[0-9.]*'   | sort -uV | tail -1
```

### 3. Install the package

```sh
sudo apt install ./dist/ml2165-rust_1.1.0-1_amd64.deb
```

Use `apt install ./…` rather than `dpkg -i` so that `cups` and `cups-filters`
are pulled in if they are missing.

The package installs two files — `/usr/lib/cups/filter/rastertospl-rust` and
`/usr/share/ppd/ml2165-rust/samsung-ml2165.ppd` — plus the usual documentation
under `/usr/share/doc/`. It does **not** create a print queue; that is step 5.

### 4. Find your printer's device URI

```sh
lpinfo -v
```

Copy the URI from the second column of the line matching your printer. For a
USB ML-2165 it looks like `usb://Samsung/ML-2165%20Series?serial=...`; the
serial number is your printer's own, so the line has to be read off `lpinfo -v`
rather than copied from here:

```sh
DEVICE_URI="usb://Samsung/ML-2165%20Series?serial=Z1A2B3C4D5"
```

Keep it quoted everywhere: `usb://` URIs contain `?` and `&`, which the shell would otherwise interpret.

> [!NOTE]
> Pick the device URI yourself rather than letting anything auto-detect it. CUPS device discovery is unauthenticated: a USB device reports its own manufacturer and model in its descriptor strings, so anything plugged in can claim to be a "Samsung ML-2165" and be wired up as the print destination, silently receiving your documents. Reading `lpinfo -v` and choosing the line yourself is the review step that prevents this.

### 5. Register the CUPS queue

Because the package puts the PPD under `/usr/share/ppd/`, CUPS lists it as a
model in its own driver database, so the queue can name it by model instead of
by path:

```sh
lpinfo -m | grep -i ml-2165      # ml2165-rust/samsung-ml2165.ppd …
sudo lpadmin -p ML2165_Rust -E -v "$DEVICE_URI" \
    -m ml2165-rust/samsung-ml2165.ppd
```

`ML2165_Rust` is the queue name and is yours to choose; CUPS allows at most 127 characters and rejects spaces, `/` and `#`, so stick to letters, digits, `_`, `.` and `-`.

Then send a test print:

```sh
lp -d ML2165_Rust file.pdf
```

The same driver shows up in the graphical tools (CUPS' web interface at
<http://localhost:631> → *Administration* → *Add Printer*, or GNOME/KDE printer
settings) as *Samsung ML-2165 Series, Rust SPL2 Driver*. What those dialogs save
you is the typing, not the choice: they show the detected USB device as a line
you click, which is the same deliberate pick the note in step 4 asks for.

## Manual installation

Use this route only if the package is not an option — another distribution, or
a working copy of the filter you are still changing. It installs the same two
files by hand, which means `apt` cannot remove them again; see
[Uninstallation](#uninstallation).

Steps 1 and 4 of the package route above still apply: check the repository
ownership first, and have `$DEVICE_URI` ready.

### 1. Build the filter, install it, validate the PPD

```sh
cargo build --release
sudo install -m 755 -o root -g root \
    target/release/rastertospl-rust /usr/lib/cups/filter/rastertospl-rust
cupstestppd ppd/samsung-ml2165.ppd
```

- **Use `install`, not `cp`.** The `-m 755 -o root -g root` flags matter: a filter that is writable by a non-root user is a filter someone else can replace.
- **Run `cupstestppd` after the binary is in place.** It checks that the file referenced by the PPD's `cupsFilter`/`cupsFilter2` line actually exists, so running it first reports a failure that isn't real.

### 2. Register the CUPS queue against the PPD in the repository

```sh
sudo lpadmin -p ML2165_Rust -E -v "$DEVICE_URI" -P ppd/samsung-ml2165.ppd
```

`-P` copies the PPD into `/etc/cups/ppd/` for that queue, so the queue keeps
working if the repository moves — but CUPS will not list this driver as a model
for other queues the way the package does.

## Print options

Beyond page size and resolution, the PPD exposes two options that the filter
reads from the CUPS raster page header and forwards to the printer:

```sh
lp -d ML2165_Rust -o InputSlot=Manual -o MediaType=ENV envelope.pdf
lpoptions -d ML2165_Rust -l            # list every option and its choices
```

- **`InputSlot`** — `Auto` (the cassette) or `Manual` (the manual feed slot).
  The PPD numbers these with the QPDL paper-source codes themselves
  (`<</MediaPosition 1>>` and `2`), which the filter writes straight into byte
  `0x9` of the QPDL page header.
- **`MediaType`** — `OFF`, `NORMAL`, `THICK`, `THIN`, `BOND`, `OHP`, `CARD`,
  `LABEL`, `USED`, `COLOR`, `ENV`, `COTTON`, `RECYCLED`, `ARCHIVE`. These
  uppercase keywords look unfriendly because they are not labels: each one is
  the literal value sent as `@PJL SET PAPERTYPE=...`, taken from the
  `*MediaType` list in upstream SpliX's PPDs for this engine family
  (`ml1910.ppd`, `ml2010.ppd`, `ml2525.ppd`, `ml1640.ppd`, `ml2510.ppd`). The
  printer picks its fuser temperature and feed speed from this, so it is worth
  setting for envelopes, labels and card stock. `OFF` is the default and means
  "use the printer's own setting". Anything the filter does not recognise falls
  back to `OFF` and is reported on stderr, so a stale PPD shows up in
  `/var/log/cups/error_log` rather than silently printing envelopes on
  plain-paper settings.

> [!NOTE]
> The filter accepts only the page sizes and resolutions this PPD declares
> (paper dimensions may also have their width and height swapped for landscape).
> A page geometry that matches neither a `*PaperDimension` entry nor its
> landscape rotation, or a resolution pair other than 300x300, 600x600,
> 1200x600 and 1200x1200, fails the job with an `ERROR: … Desteklenmeyen …`
> line in `/var/log/cups/error_log` instead of printing (the filter's diagnostics
> are in Turkish). Earlier versions substituted A4 while logging a warning and
> coerced each unsupported resolution axis independently to 300, 600 or 1200
> DPI. That could send the printer a paper code or DPI that did not match the
> raster geometry it was being handed — misaligned output, or a feed from the
> wrong tray. Such a failure usually means that the queue is using a different
> PPD than `ppd/samsung-ml2165.ppd`, or that it received a malformed raster
> stream; first re-run the `lpadmin` command from step 5 to confirm the PPD.

> [!IMPORTANT]
> If you installed an earlier version of this PPD, re-run the `lpadmin` command
> from step 5 to load the current one. The older PPD offered paper types under
> readable names (`Plain`, `Envelope`, …) that never reached the printer, and a
> third paper source ("Tray 1") that does not exist on this hardware. Saved
> defaults referring to those names (`lpoptions -o MediaType=Plain`) are no
> longer valid choices and should be set again.

## Uninstallation

### 1. Remove the queue

```sh
lpstat -p                       # if you're unsure of the name
sudo lpadmin -x ML2165_Rust
```

### 2. Remove the driver, but only once nothing still uses it

The filter is shared by every queue built on this driver, so removing it while
another one is still installed breaks that queue silently — its jobs start
failing with "filter failed". Ask which installed PPDs still name the filter:

```sh
sudo grep -rlsF rastertospl-rust /etc/cups/ppd/
```

`lpadmin -x` already deleted the removed queue's own PPD, so it won't appear
here. If the command prints nothing, no queue needs the filter any more. If the
driver came from the package, that is all `apt` needs:

```sh
sudo apt remove ml2165-rust
```

If it was installed by hand, delete the binary the same way:

```sh
sudo rm -f /usr/lib/cups/filter/rastertospl-rust
```

Note that the question is which PPD references the filter, not which queue looks
like a Samsung: the model string in a `usb://` URI is reported by the device
itself, so it says nothing about which driver the queue was built with.

## Testing

```sh
cargo test
```

106 unit tests cover the CUPS Raster parser (v1/v2/v3, both endiannesses, the
v2 line-RLE decoder), page-header validation, the SPL2/QPDL record layout, the
horizontal band placement, the Algo 0x11 RLE round trip, PJL field sanitisation,
job cancellation (SIGTERM before, between and in the middle of pages), and every
resource limit the filter enforces.

Several of them are pinned against measurements from real `cupsfilter` output
rather than from the specification — `test_validate_page_header_accepts_real_cupsfilter_heights`
carries the observed `cupsHeight` for each paper size and resolution (A4 at
600 DPI is ~6817 lines, not the 7017 the page dimensions alone suggest, because
the PPD's `*ImageableArea` margins come off first). Those numbers were measured
while the margins were 12 pt; they are now 12.5 pt per side, matching upstream
SpliX, so real output is a few lines shorter and the table stays a valid upper
bound. Keep that table measured, not computed, if you extend it.

There is no end-to-end script. To check the filter against the real CUPS
toolchain by hand:

```sh
cargo build --release
/usr/sbin/cupsfilter -p ppd/samsung-ml2165.ppd -m application/vnd.cups-raster -- doc.pdf > test.raster
./target/release/rastertospl-rust 101 testuser Test 1 "" test.raster > out.spl
```

A well-formed `out.spl` starts with `\x1b%-12345X@PJL`, contains
`@PJL ENTER LANGUAGE = QPDL`, and ends with `\t\x1b%-12345X`.

## Project Structure

- `src/main.rs` — CUPS filter entry point: argument parsing, page/band loop
- `src/raster.rs` — CUPS Raster (V1/V2/V3) header parser
- `src/spl.rs` — SPL2/QPDL protocol: PJL envelope, page/band records, Algo 0x11 RLE
- `src/cancel.rs` — SIGTERM handling, so a cancelled job still ends with the closing UEL
- `ppd/samsung-ml2165.ppd` — CUPS PPD file
- `packaging/debian/` — `control`, `copyright` and `changelog` for the `.deb`

## License

GPLv2 (v2 only) — see [LICENSE](LICENSE). The protocol implementation is derived from the GPLv2-licensed OpenPrinting SpliX project, so it's licensed to match.
