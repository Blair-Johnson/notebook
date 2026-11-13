# nb

Jupyter notebooks in the terminal, with vim keys.

## Install

```sh
cargo install --path crates/notebook --locked   # puts `nb` in ~/.cargo/bin
```

## Use it in a project

The kernel runs in your project's environment, which needs `ipykernel`
(it brings IPython, jupyter_client and pyzmq, and registers the `python3`
kernel):

```sh
pixi add ipykernel
pixi shell
nb analysis.ipynb      # open or create a notebook
nb quick_test.py       # a percent-format script: cells start with `# %%`
nb                     # a scratch notebook
```

`pixi run nb file.py` works too. `nb` finds the kernel through the active
environment (`$CONDA_PREFIX`, `$VIRTUAL_ENV`), then a `.venv/` or `venv/`
next to the file, then the standard Jupyter paths.

## Options

| Option | Meaning |
|---|---|
| `--kernel NAME` | Use a different kernelspec (default: the notebook's, then `python3`) |
| `--existing FILE` | Attach to a running kernel's connection file; quitting leaves it running |
| `--config FILE` | Use this config file |

Settings live in `~/.config/notebook/config.toml` (or `$NB_CONFIG`), and
are easiest to change from the app: `Ctrl+/` searches commands and
settings, `Ctrl+,` opens the settings screen, `?` lists keys.

Quit with `:q`, `:wq` or `:q!`, as in vim.
