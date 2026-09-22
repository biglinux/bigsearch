# Arch package build

Build from this directory, not from the project root:

```sh
cd packaging/arch
makepkg -fi
```

This keeps makepkg's temporary `src/` and `pkg/` directories under
`packaging/arch/`, avoiding a collision with the Rust crate's real `src/`
directory.

To build a different checkout:

```sh
BIG_SEARCH_SOURCE=/path/to/big-search makepkg -fi
```
