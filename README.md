# nb

![nb showing a notebook with code cells and their outputs](docs/images/screenshot.png)

Jupyter notebooks in the terminal, with vim keys.

## Install

You need a Rust toolchain, version 1.90 or newer (`rustup update`).

```sh
git clone <this repo> notebook && cd notebook
cargo install --path crates/notebook --locked
```

This puts `nb` in `~/.cargo/bin`. Run the same command again to update.

## Use

Each project's environment needs `ipykernel`, which provides the Python
kernel:

```sh
pixi add ipykernel
pixi shell
nb analysis.ipynb      # open or create a notebook
nb quick_test.py       # percent-format script: cells start with `# %%`
```

`pixi run nb analysis.ipynb` also works, as does any conda or venv
environment with `ipykernel` installed.

Inside: `?` lists keys, `Ctrl+/` searches commands and settings, and
`:w`, `:wq`, `:q!` work as in vim. `nb --help` lists the command-line
options.
