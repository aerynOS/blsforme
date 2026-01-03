# blsforme

A management tool and library enabling Linux distributions to more easily adopt the [Boot Loader Specification](https://uapi-group.org/specifications/specs/boot_loader_specification).

 - Discovery of ESP through [Boot Loader Interface](https://systemd.io/BOOT_LOADER_INTERFACE/) - allow suppression of ESP & XBOOTLDR automounting to alleviate weak data integrity concerns.
 - Automatic promotion of kernels/initrds to `$BOOT`
 - Synchronise `$BOOT` state with the source intent, i.e. mirroring `/usr/lib/kernel` to facilitate garbage collection.
 - Cascading filesystem policy allowing vendor overrides/masking for initrds, cmdlines, etc.
 - Heavy focus on enabling `type 1` BLS entries with automatic `root=` cmdline generation.
 - `XBOOTLDR` support per the [Discoverable Partitions Specification](https://www.freedesktop.org/wiki/Specifications/DiscoverablePartitionsSpec/)
 - Concrete policy for kernel packaging, prebuilt vendor initrds, etc.
 - Rudimentary fallback for non-UEFI cases (GRUB2 chained bootloader)

Primarily this tooling has been designed to assist the [moss](https://github.com/aerynos/os-tools) package manager, but will remain agnostic to support the use case of [Solus](https://getsol.us) and other interested parties.

## Testing

```bash
# Update your clone with git pull
# Build fresh release
cargo build
# Show commit of the build
git log -1
# Run blsforme test
sudo RUST_LOG=trace ./target/debug/blsctl status
# Compare to existing boot arguments
cat /proc/cmdline
# Compare blsforme discovery to blkid discovery
sudo lsblk -f
```

Paste a suitable excerpt of the above to e.g. https://bpa.st and link to it in the 'General - AerynOS' channel on Zulip

## Difference to alternatives

As the original author of [clr-boot-manager](https://github.com/clearlinux/clr-boot-manager) it needs listing here as "prior art", in terms of synchronising `$BOOT` and `/usr/lib/kernel` for type 1 BLS entries.

However the original design has a number of weaknesses and doesn't provide a sane schema for the automated discovery of kernel assets without a compile-time vendor prefix.

In a similar vein, [kernel-install](https://www.freedesktop.org/software/systemd/man/latest/kernel-install.html) is very fuzzy on type 1 vendoring and instead relies on plugins to generate an initramfs (or indeed a staging directory for dracut via a package trigger).

Additionally `kernel-install` is designed to be a one-shot utility invoked by packaging triggers (or users) rather than a more contained facility to synchronise the target `ESP` (or `$BOOT`) with the expected state as provided by the final package-managed state.


In the scope of [AerynOS](https://aerynos.com/) and [Solus](https://getsol.us) - prebuilt initrds have been in use for years with great success. Given the requirement for both distributions to function correctly in dual-boot and non-appliance use cases, a `.uki` isn't going to permit our use case of generating dynamic cmdlines and shipping pre-signed assets.

## Filesystem layout

For discovery to work, `blsforme` expects kernels to live in versioned directories under `/usr/lib/kernel`:

```bash
    /usr/lib/kernel
        6.8.9-289.current/
            vmlinuz # Kernel boot image

            boot.json # Kernel manifest

            # Version specific files.
            10-default.initrd
            10-default.cmdline

        initrd.d/
            # Non-version specific initrd
            01-firmware.initrd

        cmdline.d/
            99-global.cmdline

    /etc/kernel
        initrd.d/
            # Non-version specific
            ...
        cmdline.d/
            00-local.cmdline

        cmdline -> cmdline.d/00-local.cmdline
```

## `boot.json`

To further facilitate the development of utilities to enumerate and manipulate boot entries, we augment the kernel packages with a JSON file. Right now this is a developing format which primarily lists the **variant** of the kernel, allowing users to set their preferred default variant when updating/manipulating kernels. As an example, `lts` vs `mainline`.

```json
{
    "name": "linux-current",
    "version": "6.8.9-289.current", /* uname -r */
    "variant": "lts", /* effectively a grouping key. */
}
```
## License

`blsforme` is available under the terms of the [MPL-2.0](https://spdx.org/licenses/MPL-2.0.html)
