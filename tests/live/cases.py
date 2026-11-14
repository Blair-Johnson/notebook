"""Expected behavior of every default binding, by mode.

A case: name, fixture, optional setup keys, the keys under test, and a check
`(before, after, screen, alive) -> (ok, detail)`. Expectations come from vim
semantics and nb's documented behavior, written here by hand.

`terminal=True` marks keys most terminals (and tmux 3.3) cannot send, which
the Controls tab flags with a warning: their failure is reported separately.
"""

import json

# ---------------------------------------------------------------- fixtures

LINE0 = 'alpha = beta(gamma, "delta")'
LINE1 = "    second_line  word"
LINE2 = "third.line = [1, 2]"
LINE4 = "fifth = (x)"
SOURCE0 = "\n".join([LINE0, LINE1, LINE2, "", LINE4])
OUT0 = "out line (zero)\nout line one two\nout three\n"
OUT_LINE0 = "out line (zero)"
TRACEBACK = ["Traceback (most recent call last):", "Cell In[1], line 2", "NameError: name 'x' is not defined"]


def ipynb(cells):
    out = []
    for kind, source, outputs, count in cells:
        cell = {"cell_type": kind, "metadata": {}, "source": source}
        if kind == "code":
            cell["execution_count"] = count
            cell["outputs"] = outputs
        out.append(cell)
    return json.dumps(
        {
            "cells": out,
            "metadata": {"kernelspec": {"name": "python3", "display_name": "Python 3", "language": "python"}},
            "nbformat": 4,
            "nbformat_minor": 5,
        }
    )


def stream(name, text):
    return {"output_type": "stream", "name": name, "text": text}


def error(lines):
    return {"output_type": "error", "ename": "NameError", "evalue": "x", "traceback": lines}


# cell 0: text with output; 1: no output; 2: markdown; 3: stdout + stderr; 4: empty.
MAIN = (
    "main.ipynb",
    ipynb(
        [
            ("code", SOURCE0, [stream("stdout", OUT0)], 1),
            ("code", "y = 2", [], None),
            ("markdown", "# Title\ntext", [], None),
            ("code", "z = 3", [stream("stdout", "z out\n"), stream("stderr", "z err\n")], 3),
            ("code", "", [], None),
        ]
    ),
)

OBJ_LINE = "f('sq', `bt`, {k: v}, <ab>)"
OBJECTS = ("objects.ipynb", ipynb([("code", OBJ_LINE + "\nsecond\n\nnext para", [], None)]))

TRACE = (
    "trace.ipynb",
    ipynb(
        [
            ("code", "a = 1\n    b = x", [], 1),
            ("code", "y = 2", [error(TRACEBACK)], 2),
        ]
    ),
)

KERNEL = ("kernel.ipynb", ipynb([("code", "print(6 * 7)", [], None), ("code", "w = 5", [], None)]))
SLEEP = ("sleep.ipynb", ipynb([("code", "print('started', flush=True)\nimport time\ntime.sleep(30)", [], None)]))
PDB = (
    "pdb.ipynb",
    ipynb([("code", 'a = input("first? ")\nb = input("second? ")\nprint(a, b)', [], None)]),
)
FORMAT = ("format.ipynb", ipynb([("code", "x=1", [], None)]))
LONG = ("long.ipynb", ipynb([("code", f"v{n} = {n}", [], None) for n in range(40)]))

# ---------------------------------------------------------------- helpers


def ok(cond, detail):
    return (bool(cond), "" if cond else detail)


def cell(state, index=None):
    if index is None:
        index = state["selected_cell"]
    return state["cells"][index]


def mode(state):
    if state is None:
        return "gone"
    layer = state["layers"][-1]
    return layer.get("mode") or layer.get("kind") or layer["layer"]


def cursor(state):
    """The cursor of the text being edited or read."""
    if mode(state).startswith("output"):
        return tuple(state["output_text"]["cursor"])
    layer = next((l for l in reversed(state["layers"]) if l["layer"] == "cell"), None)
    index = layer["cell"] if layer else state["selected_cell"]
    return tuple(state["cells"][index]["cursor"])


def selection(state):
    if mode(state).startswith("output"):
        return state["output_text"]["selection"]
    return cell(state)["selection"]


def lines(state, index=None):
    return cell(state, index)["source"].split("\n")


def source(state, index=None):
    return cell(state, index)["source"]


def check(cond, detail):
    def run(b, a, s, alive):
        if a is None:
            return False, "no state"
        return ok(cond(b, a), detail(b, a) if callable(detail) else detail)

    return run


def all_of(*checks):
    def run(b, a, s, alive):
        for c in checks:
            good, detail = c(b, a, s, alive)
            if not good:
                return good, detail
        return True, ""

    return run


def at(pos, want_mode=None):
    def run(b, a, s, alive):
        if want_mode and mode(a) != want_mode:
            return False, f"mode {mode(a)}, wanted {want_mode}; cursor {cursor(a)}"
        return ok(cursor(a) == tuple(pos), f"cursor {cursor(a)}, wanted {tuple(pos)}")

    return run


def row_is(row, want_mode=None):
    def run(b, a, s, alive):
        if want_mode and mode(a) != want_mode:
            return False, f"mode {mode(a)}, wanted {want_mode}"
        return ok(cursor(a)[0] == row, f"cursor {cursor(a)}, wanted row {row}")

    return run


def in_mode(want):
    return lambda b, a, s, alive: ok(mode(a) == want, f"mode {mode(a)}, wanted {want}")


def line_is(row, text, want_mode=None, index=None):
    def run(b, a, s, alive):
        if want_mode and mode(a) != want_mode:
            return False, f"mode {mode(a)}, wanted {want_mode}; lines {lines(a, index)[:3]}"
        got = lines(a, index)
        return ok(len(got) > row and got[row] == text, f"line {row} is {got[row] if len(got) > row else None!r}, wanted {text!r}")

    return run


def register(text, linewise=None):
    def run(b, a, s, alive):
        reg = a["register"]
        if linewise is not None and reg["linewise"] != linewise:
            return False, f"register {reg}, wanted linewise={linewise}"
        return ok(reg["text"] == text, f"register {reg['text']!r}, wanted {text!r}")

    return run


def unchanged_doc():
    return lambda b, a, s, alive: ok(
        [c["source"] for c in a["cells"]] == [c["source"] for c in b["cells"]] and len(a["cells"]) == len(b["cells"]),
        "the document changed",
    )


def quits():
    return lambda b, a, s, alive: ok(not alive or (a and a["should_quit"]), "nb is still running")


def modal(kind):
    return lambda b, a, s, alive: ok(
        a["layers"][-1].get("kind") == kind, f"top layer {a['layers'][-1]}, wanted modal {kind}"
    )


CASES = []


def case(name, keys, check, setup="", fixture=MAIN, **extra):
    CASES.append(dict(name=name, keys=keys, check=check, setup=setup, fixture=fixture, **extra))


# How to reach each mode from the notebook, on cell 0.
ENTER = {
    "notebook.normal": "",
    "cell.normal": "i",
    "cell.visual": "iv",
    "cell.visual_line": "iV",
    "cell.insert": "ii",
    "output.normal": "zo",
    "output.visual": "zov",
    "output.visual_line": "zoV",
}

# ---------------------------------------------------------------- motions in text

# (action, keys, keys before the motion, expected cursor in cell 0's source)
CELL_MOTIONS = [
    ("motion.left", ["h", "<left>", "<bs>"], "ll", (0, 1)),
    ("motion.right", ["l", "<right>", "<space>"], "", (0, 1)),
    ("motion.down", ["j", "<down>"], "", (1, 0)),
    ("motion.up", ["k", "<up>"], "j", (0, 0)),
    ("motion.word_next", ["w"], "", (0, 6)),
    ("motion.word_back", ["b"], "llllllll", (0, 6)),
    ("motion.word_end", ["e"], "", (0, 4)),
    ("motion.bigword_next", ["W"], "llllllll", (0, 20)),
    ("motion.bigword_back", ["B"], "$", (0, 20)),
    ("motion.bigword_end", ["E"], "llllllll", (0, 18)),
    ("motion.line_start", ["0", "<home>"], "llllllll", (0, 0)),
    ("motion.line_first_nonblank", ["^"], "j$", (1, 4)),
    ("motion.current_line", ["_"], "j$", (1, 4)),
    ("motion.line_end", ["$", "<end>"], "", (0, 27)),
    ("motion.paragraph_next", ["}"], "", (3, 0)),
    ("motion.paragraph_prev", ["{"], "G", (3, 0)),
    ("motion.match_pair", ["%"], "llllllllllll", (0, 27)),
    ("motion.find_forward", ["fd"], "", (0, 21)),
    ("motion.find_backward", ["Fa"], "$", (0, 25)),
    ("motion.till_forward", ["td"], "", (0, 20)),
    ("motion.till_backward", ["Ta"], "$", (0, 26)),
    ("motion.repeat_find", [";"], "fa", (0, 11)),
    ("motion.repeat_find_reverse", [","], "fa;", (0, 4)),
    ("motion.top", ["gg"], "G", (0, 0)),
    ("motion.bottom", ["G"], "", (4, 0)),
    ("motion.half_page_down", ["<c-d>", "<pgdn>"], "", (4, 0)),
    ("motion.half_page_up", ["<c-u>", "<pgup>"], "G", (0, 0)),
]

# The same motions over cell 0's output text.
OUTPUT_MOTIONS = [
    ("motion.left", ["h", "<left>", "<bs>"], "ll", (0, 1)),
    ("motion.right", ["l", "<right>", "<space>"], "", (0, 1)),
    ("motion.down", ["j", "<down>"], "", (1, 0)),
    ("motion.up", ["k", "<up>"], "j", (0, 0)),
    ("motion.word_next", ["w"], "", (0, 4)),
    ("motion.word_back", ["b"], "llll", (0, 0)),
    ("motion.word_end", ["e"], "", (0, 2)),
    ("motion.bigword_next", ["W"], "", (0, 4)),
    ("motion.bigword_back", ["B"], "$", (0, 9)),
    ("motion.bigword_end", ["E"], "", (0, 2)),
    ("motion.line_start", ["0", "<home>"], "llll", (0, 0)),
    ("motion.line_first_nonblank", ["^"], "llll", (0, 0)),
    ("motion.current_line", ["_"], "llll", (0, 0)),
    ("motion.line_end", ["$", "<end>"], "", (0, 14)),
    ("motion.paragraph_next", ["}"], "", (2, 8)),
    ("motion.paragraph_prev", ["{"], "G", (0, 0)),
    ("motion.match_pair", ["%"], "lllllllll", (0, 14)),
    ("motion.find_forward", ["f("], "", (0, 9)),
    ("motion.find_backward", ["Fo"], "$", (0, 13)),
    ("motion.till_forward", ["t("], "", (0, 8)),
    ("motion.till_backward", ["Tl"], "$", (0, 5)),
    ("motion.repeat_find", [";"], "fe", (0, 11)),
    ("motion.repeat_find_reverse", [","], "fe;", (0, 7)),
    ("motion.top", ["gg"], "G", (0, 0)),
    ("motion.bottom", ["G"], "", (2, 0)),
    ("motion.half_page_down", ["<c-d>", "<pgdn>"], "", (2, 0)),
    ("motion.half_page_up", ["<c-u>", "<pgup>"], "G", (0, 0)),
]


def motion_cases(modes, table):
    for m in modes:
        for action, keyset, before, pos in table:
            visual = "visual" in m
            # Visual modes: move first, then start the selection, so the
            # motion extends it from a known anchor.
            base = ENTER[m].rstrip("vV") if visual else ENTER[m]
            vkey = ENTER[m][-1] if visual else ""
            for key in keyset:
                checks = [at(pos, m)]
                if visual:
                    checks.append(
                        lambda b, a, s, al: ok(selection(a) is not None, "no selection")
                    )
                case(f"{m}.{action} [{key}]", key, all_of(*checks), setup=base + before + vkey)


motion_cases(["cell.normal", "cell.visual", "cell.visual_line"], CELL_MOTIONS)
motion_cases(["output.normal", "output.visual", "output.visual_line"], OUTPUT_MOTIONS)

# ---------------------------------------------------------------- shared app keys (every mode)

APP_MODES = ["notebook.normal", "cell.normal", "cell.insert", "cell.visual", "cell.visual_line", "output.normal", "output.visual", "output.visual_line"]
for m in APP_MODES:
    setup = ENTER[m]
    quit_key = "q" if m == "notebook.normal" else "<c-q>"
    case(f"{m}.app.quit [{quit_key}]", quit_key, quits(), setup=setup)
    case(f"{m}.app.palette [<c-/>]", "<c-/>", modal("CommandPalette"), setup=setup)
    case(f"{m}.app.settings [<c-,>]", "<c-,>", modal("Settings"), setup=setup, terminal=True)
    case(
        f"{m}.ui.toggle_mouse_capture [<a-m>]",
        "<a-m>",
        check(lambda b, a: a["mouse_capture"] != b["mouse_capture"], "mouse capture did not toggle"),
        setup=setup,
    )
    if m != "cell.insert":
        case(f"{m}.app.help [?]", "?", modal("Help"), setup=setup)
        case(f"{m}.app.command_bar [:]", ":", check(lambda b, a: a["prompt"] and a["prompt"]["kind"] == "ExCommand", "no : prompt"), setup=setup)
        case(f"{m}.search.open [/]", "/", check(lambda b, a: a["prompt"] and a["prompt"]["kind"] == "Search", "no / prompt"), setup=setup)

# ctrl+c interrupts a running cell from every mode.
for m in APP_MODES:
    enter = ENTER[m]
    # Run the sleeping cell first, from the notebook, then enter the mode
    # (its output, "started", lets the output modes enter too).
    case(
        f"{m}.kernel.interrupt [<c-c>]",
        "<c-c>",
        check(
            lambda b, a: any("KeyboardInterrupt" in o for o in a["cells"][0]["outputs"]),
            lambda b, a: f"outputs {a['cells'][0]['outputs']}",
        ),
        setup="<cr><wait:1500>" + enter,
        fixture=SLEEP,
        kernel=True,
        wait_until=lambda s: any("KeyboardInterrupt" in o for o in s["cells"][0]["outputs"]),
        wait_limit=10,
    )

# ---------------------------------------------------------------- search inside text

for m in ["cell.normal", "cell.visual", "cell.visual_line"]:
    base = ENTER[m]
    case(f"{m}.search.open finds in the cell [/]", "/second<cr>", at((1, 4), m), setup=base)
    case(f"{m}.search.next [n]", "n", at((2, 2), m), setup=base + "/i<cr>")
    case(f"{m}.search.prev [N]", "N", at((1, 12), m), setup=base + "/i<cr>n")
for m in ["output.normal", "output.visual", "output.visual_line"]:
    base = ENTER[m]
    case(f"{m}.search.open finds in the output [/]", "/three<cr>", at((2, 4), m), setup=base)
    case(f"{m}.search.next [n]", "n", at((2, 0), m), setup=base + "/out<cr>")
    case(f"{m}.search.prev [N]", "N", at((1, 0), m), setup=base + "/out<cr>n")

# ---------------------------------------------------------------- cell normal

CN = "i"
case("cell.normal.cell.enter_insert [i]", "i", at((0, 0), "cell.insert"), setup=CN)
case("cell.normal.cell.append_char [a]", "a", at((0, 1), "cell.insert"), setup=CN)
case("cell.normal.edit.insert_line_start [I]", "I", at((1, 4), "cell.insert"), setup=CN + "j$")
case("cell.normal.edit.append_line_end [A]", "A", at((0, 28), "cell.insert"), setup=CN)
case("cell.normal.edit.open_below [o]", "o", all_of(at((2, 4), "cell.insert"), line_is(2, "    ")), setup=CN + "j")
case("cell.normal.edit.open_above [O]", "O", all_of(at((1, 4), "cell.insert"), line_is(1, "    "), line_is(2, LINE1)), setup=CN + "j")
for key in ["x", "<del>"]:
    case(f"cell.normal.edit.delete_char [{key}]", key, line_is(0, LINE0[1:], "cell.normal"), setup=CN)
case("cell.normal.edit.delete_char_back [X]", "X", all_of(line_is(0, LINE0[1:], "cell.normal"), at((0, 0))), setup=CN + "l")
case("cell.normal.edit.substitute [s]", "s", line_is(0, LINE0[1:], "cell.insert"), setup=CN)
case("cell.normal.edit.substitute_line [S]", "S", all_of(line_is(0, "", "cell.insert"), line_is(1, LINE1)), setup=CN)
case("cell.normal.edit.delete_to_eol [D]", "D", line_is(0, "alpha = ", "cell.normal"), setup=CN + "llllllll")
case("cell.normal.edit.change_to_eol [C]", "C", all_of(line_is(0, "alpha = ", "cell.insert"), at((0, 8))), setup=CN + "llllllll")
case("cell.normal.edit.replace_char [r]", "rz", line_is(0, "z" + LINE0[1:], "cell.normal"), setup=CN)
case("cell.normal.edit.toggle_case [~]", "~", line_is(0, "A" + LINE0[1:], "cell.normal"), setup=CN)
case("cell.normal.edit.join_lines [J]", "J", line_is(0, LINE0 + " " + LINE1.strip(), "cell.normal"), setup=CN)
case("cell.normal.edit.paste_after [p]", "p", line_is(0, "allpha" + LINE0[5:], "cell.normal"), setup=CN + "lylh")
case("cell.normal.edit.paste_before [P]", "P", line_is(0, "lalpha" + LINE0[5:], "cell.normal"), setup=CN + "lylh")
case("cell.normal.edit.undo [u]", "u", line_is(0, LINE0, "cell.normal"), setup=CN + "x")
case("cell.normal.edit.redo [<c-r>]", "<c-r>", line_is(0, LINE0[1:], "cell.normal"), setup=CN + "xu")
case("cell.normal.edit.yank_line [Y]", "Y", register(LINE0 + "\n", True), setup=CN)
case("cell.normal.op.delete [dw]", "dw", line_is(0, '= beta(gamma, "delta")', "cell.normal"), setup=CN)
case("cell.normal.op.delete [dd]", "dd", line_is(0, LINE1, "cell.normal"), setup=CN)
case("cell.normal.op.change [cw]", "cw", all_of(line_is(0, ' = beta(gamma, "delta")', "cell.insert"), at((0, 0))), setup=CN)
case("cell.normal.op.yank [yw]", "yw", all_of(register("alpha "), unchanged_doc()), setup=CN)
case("cell.normal.op.indent [>>]", ">>", line_is(0, "    " + LINE0, "cell.normal"), setup=CN)
case("cell.normal.op.dedent [<lt><lt>]", "<lt><lt>", line_is(1, LINE1.strip(), "cell.normal"), setup=CN + "j")
case("cell.normal.visual.char [v]", "v", in_mode("cell.visual"), setup=CN)
case("cell.normal.visual.line [V]", "V", in_mode("cell.visual_line"), setup=CN)
case("cell.normal.focus.up [esc]", "<esc>", in_mode("notebook.normal"), setup=CN)
case(
    "cell.normal.cell.toggle_expand_split [<c-o>]",
    "<c-o>",
    check(lambda b, a: a["layers"][-1].get("expanded_split") is True, "the editor did not expand"),
    setup=CN,
)
case(
    "cell.normal.cell.split_at_cursor [<c-\\>]",
    "<c-\\>",
    check(lambda b, a: len(a["cells"]) == 6 and a["cells"][1]["source"].startswith(LINE1), lambda b, a: f"{len(a['cells'])} cells: {[c['source'][:12] for c in a['cells'][:2]]}"),
    setup=CN + "j",
)
case(
    "cell.normal.cell.edit_external [<c-g>]",
    "<c-g>",
    check(lambda b, a: a["cells"][0]["source"].endswith("# edited"), lambda b, a: f"source ends {a['cells'][0]['source'][-20:]!r}"),
    setup=CN,
)
case(
    "cell.normal.cell.run [enter]",
    "<cr>",
    check(lambda b, a: "42" in "".join(a["cells"][0]["outputs"]), lambda b, a: f"outputs {a['cells'][0]['outputs']}"),
    setup=CN,
    fixture=KERNEL,
    kernel=True,
    wait_until=lambda s: "42" in "".join(s["cells"][0]["outputs"]),
)
case(
    "cell.normal.intel.inspect_doc [K]",
    "K",
    check(lambda b, a: a["inspect"] is not None, "no documentation pager"),
    setup=CN,
    fixture=KERNEL,
    kernel=True,
    wait_until=lambda s: s["inspect"] is not None,
)

# ---------------------------------------------------------------- text objects (after an operator)

OBJ = [
    # (keys after `d`, keys to place the cursor, expected first line)
    ("iw", "", "('sq', `bt`, {k: v}, <ab>)"),
    ("aw", "", "('sq', `bt`, {k: v}, <ab>)"),
    ("iW", "", " `bt`, {k: v}, <ab>)"),
    ("aW", "", "`bt`, {k: v}, <ab>)"),
    ("i'", "lll", "f('', `bt`, {k: v}, <ab>)"),
    ("a'", "lll", "f(, `bt`, {k: v}, <ab>)"),
    ("i`", "lllllllll", "f('sq', ``, {k: v}, <ab>)"),
    # No white space after the closing quote: the space before goes too.
    ("a`", "lllllllll", "f('sq',, {k: v}, <ab>)"),
    ("i(", "lll", "f()"),
    ("i)", "lll", "f()"),
    ("ib", "lll", "f()"),
    ("a(", "lll", "f"),
    ("a)", "lll", "f"),
    ("ab", "lll", "f"),
    ("i{", "lllllllllllllll", "f('sq', `bt`, {}, <ab>)"),
    ("i}", "lllllllllllllll", "f('sq', `bt`, {}, <ab>)"),
    ("iB", "lllllllllllllll", "f('sq', `bt`, {}, <ab>)"),
    ("a{", "lllllllllllllll", "f('sq', `bt`, , <ab>)"),
    ("a}", "lllllllllllllll", "f('sq', `bt`, , <ab>)"),
    ("aB", "lllllllllllllll", "f('sq', `bt`, , <ab>)"),
    ("i<lt>", "lllllllllllllllllllllll", "f('sq', `bt`, {k: v}, <>)"),
    ("i>", "lllllllllllllllllllllll", "f('sq', `bt`, {k: v}, <>)"),
    ("a<lt>", "lllllllllllllllllllllll", "f('sq', `bt`, {k: v}, )"),
    ("a>", "lllllllllllllllllllllll", "f('sq', `bt`, {k: v}, )"),
    ("ip", "", ""),
    ("ap", "", "next para"),
]
for obj, place, want in OBJ:
    case(f"text.pending.d{obj}", "d" + obj, line_is(0, want, "cell.normal"), setup="i" + place, fixture=OBJECTS)
# Double quotes and brackets live in the main fixture's lines 0 and 2.
case('text.pending.di"', 'di"', line_is(0, 'alpha = beta(gamma, "")', "cell.normal"), setup="i" + "l" * 21)
case('text.pending.da"', 'da"', line_is(0, "alpha = beta(gamma,)", "cell.normal"), setup="i" + "l" * 21)
for key in ["i[", "i]"]:
    case(f"text.pending.d{key}", "d" + key, line_is(2, "third.line = []", "cell.normal"), setup="ijj" + "l" * 14)
for key in ["a[", "a]"]:
    case(f"text.pending.d{key}", "d" + key, line_is(2, "third.line = ", "cell.normal"), setup="ijj" + "l" * 14)

# ---------------------------------------------------------------- cell visual and visual line

for m, v in [("cell.visual", "v"), ("cell.visual_line", "V")]:
    sel = "i" + v
    charwise = v == "v"
    after_del = LINE0[2:] if charwise else LINE1
    for key in ["x", "<del>"]:
        case(f"{m}.edit.delete_char [{key}]", key, line_is(0, after_del, "cell.normal"), setup=sel + "l")
    for key in ["D", "X"]:
        case(f"{m}.edit.delete_to_eol [{key}]", key, line_is(0, LINE1, "cell.normal"), setup=sel + "l")
    case(f"{m}.edit.substitute [s]", "s", line_is(0, after_del if charwise else "", "cell.insert"), setup=sel + "l")
    case(f"{m}.edit.substitute_line [S]", "S", line_is(0, "", "cell.insert"), setup=sel + "l")
    case(f"{m}.edit.change_to_eol [C]", "C", line_is(0, "", "cell.insert"), setup=sel + "l")
    case(f"{m}.edit.replace_char [r]", "rz", line_is(0, ("zz" + LINE0[2:]) if charwise else "z" * len(LINE0), "cell.normal"), setup=sel + "l")
    case(f"{m}.edit.toggle_case [~]", "~", line_is(0, ("AL" + LINE0[2:]) if charwise else LINE0.upper(), "cell.normal"), setup=sel + "l")
    case(f"{m}.edit.join_lines [J]", "J", line_is(0, LINE0 + " " + LINE1.strip(), "cell.normal"), setup=sel + "j")
    case(f"{m}.edit.insert_line_start [I]", "I", at((0, 2) if charwise else (0, 0), "cell.insert"), setup="ill" + v + "ll")
    case(f"{m}.edit.append_line_end [A]", "A", at((0, 5) if charwise else (0, 28), "cell.insert"), setup="ill" + v + "ll")
    # Paste over the selection: the register replaces it.
    case(f"{m}.edit.paste_after [p]", "p", line_is(0, ("a" + LINE0[2:]) if charwise else "a", "cell.normal"), setup="iyl" + v + "l")
    case(f"{m}.edit.paste_before [P]", "P", line_is(0, ("a" + LINE0[2:]) if charwise else "a", "cell.normal"), setup="iyl" + v + "l")
    case(f"{m}.edit.undo [u]", "u", line_is(0, LINE0), setup="ix" + v)
    case(f"{m}.edit.redo [<c-r>]", "<c-r>", line_is(0, LINE0[1:]), setup="ixu" + v)
    case(f"{m}.edit.yank_line [Y]", "Y", all_of(register(LINE0 + "\n", True), in_mode("cell.normal")), setup=sel + "l")
    case(f"{m}.op.delete [d]", "d", line_is(0, after_del, "cell.normal"), setup=sel + "l")
    case(f"{m}.op.change [c]", "c", line_is(0, after_del if charwise else "", "cell.insert"), setup=sel + "l")
    case(f"{m}.op.yank [y]", "y", all_of(register("al" if charwise else LINE0 + "\n"), in_mode("cell.normal")), setup=sel + "l")
    case(f"{m}.op.indent [>]", ">", line_is(0, "    " + LINE0, "cell.normal"), setup=sel + "l")
    case(f"{m}.op.dedent [<lt>]", "<lt>", line_is(1, LINE1.strip(), "cell.normal"), setup="ij" + v + "l")
    case(f"{m}.visual.swap_ends [o]", "o", all_of(at((0, 0), m), check(lambda b, a: tuple(selection(a)["anchor"]) == (0, 2), lambda b, a: f"selection {selection(a)}")), setup=sel + "ll")
    case(f"{m}.focus.up [esc]", "<esc>", all_of(in_mode("cell.normal"), check(lambda b, a: selection(a) is None, "the selection stayed")), setup=sel)
    case(f"{m}.visual.char [v]", "v", in_mode("cell.normal" if charwise else "cell.visual"), setup=sel)
    case(f"{m}.visual.line [V]", "V", in_mode("cell.visual_line" if charwise else "cell.normal"), setup=sel)

# ---------------------------------------------------------------- cell insert

CI = "ii"
case("cell.insert.typing", "xy", line_is(0, "xy" + LINE0, "cell.insert"), setup=CI)
for key in ["<bs>", "<c-h>"]:
    case(f"cell.insert.edit.backspace [{key}]", key, line_is(0, LINE0[1:], "cell.insert"), setup="ia")
case("cell.insert.edit.delete_char [<del>]", "<del>", line_is(0, LINE0[1:], "cell.insert"), setup=CI)
case("cell.insert.edit.delete_to_line_end [<c-k>]", "<c-k>", line_is(0, "alpha = ", "cell.insert"), setup="illllllll" + "i")
case("cell.insert.edit.delete_to_line_start [<c-u>]", "<c-u>", line_is(0, "", "cell.insert"), setup="iA")
for key, terminal in [("<c-w>", False), ("<a-bs>", False), ("<c-bs>", True)]:
    case(f"cell.insert.edit.delete_word_back [{key}]", key, line_is(0, 'alpha = beta(gamma, "delta', "cell.insert"), setup="iA", terminal=terminal)
case("cell.insert.edit.indent [tab]", "<tab>", line_is(0, "    " + LINE0, "cell.insert"), setup=CI)
case("cell.insert.edit.dedent [s-tab]", "<s-tab>", line_is(1, LINE1.strip(), "cell.insert"), setup="iji")
for key in ["<cr>", "<c-j>", "<a-cr>"]:
    case(f"cell.insert.edit.newline [{key}]", key, all_of(line_is(0, "", "cell.insert"), line_is(1, LINE0)), setup=CI)
case("cell.insert.focus.up [esc]", "<esc>", at((0, 27), "cell.normal"), setup="iA")
case("cell.insert.focus.up [jj]", "jj", all_of(in_mode("cell.normal"), line_is(0, LINE0)), setup=CI)
case("cell.insert.motion.left [left]", "<left>", at((0, 27), "cell.insert"), setup="iA")
case("cell.insert.motion.right [right]", "<right>", at((0, 1), "cell.insert"), setup=CI)
case("cell.insert.motion.up [up]", "<up>", at((0, 0), "cell.insert"), setup="iji")
case("cell.insert.motion.down [down]", "<down>", at((1, 0), "cell.insert"), setup=CI)
for key in ["<home>", "<c-a>"]:
    case(f"cell.insert.motion.line_start [{key}]", key, at((0, 0), "cell.insert"), setup="iA")
for key in ["<end>", "<c-e>"]:
    case(f"cell.insert.motion.line_end [{key}]", key, at((0, 28), "cell.insert"), setup=CI)
case("cell.insert.motion.word_back [a-b]", "<a-b>", at((0, 26), "cell.insert"), setup="iA")
case("cell.insert.motion.word_next [a-f]", "<a-f>", at((0, 6), "cell.insert"), setup=CI)
case(
    "cell.insert.cell.toggle_expand_split [<c-o>]",
    "<c-o>",
    check(lambda b, a: a["layers"][-1].get("expanded_split") is True, "the editor did not expand"),
    setup=CI,
)
case(
    "cell.insert.intel.complete [<c-space>]",
    "<c-space>",
    check(lambda b, a: a["completion"], "no completion popup"),
    setup="iA<c-u>pri",
    fixture=KERNEL,
    kernel=True,
    wait_until=lambda s: s["completion"],
    wait_limit=8,
)

# ---------------------------------------------------------------- output normal and visual

for m in ["output.normal", "output.visual", "output.visual_line"]:
    setup = ENTER[m]
    charwise = m == "output.visual"
    if m == "output.normal":
        case(f"{m}.edit.yank_line [Y]", "Y", register(OUT_LINE0 + "\n", True), setup=setup)
        case(f"{m}.op.yank [yw]", "yw", register("out "), setup=setup)
        case(f"{m}.visual.char [v]", "v", in_mode("output.visual"), setup=setup)
        case(f"{m}.visual.line [V]", "V", in_mode("output.visual_line"), setup=setup)
        case(f"{m}.focus.up [esc]", "<esc>", in_mode("notebook.normal"), setup=setup)
        case(
            f"{m}.output.jump [enter]",
            "<cr>",
            all_of(at((1, 4), "cell.normal"), check(lambda b, a: a["selected_cell"] == 0, "not cell 0")),
            setup="]]zoj",
            fixture=TRACE,
        )
        case(
            f"{m}.output.next_tab [tab]",
            "<tab>",
            check(lambda b, a: "z err" in a["output_text"]["text"], lambda b, a: f"output text {a['output_text']['text']!r}"),
            setup="]]]]]]zo",
        )
    else:
        case(f"{m}.edit.yank_line [Y]", "Y", all_of(register(OUT_LINE0 + "\n", True), in_mode("output.normal")), setup=setup + "l")
        case(f"{m}.op.yank [y]", "y", all_of(register("ou" if charwise else OUT_LINE0 + "\n"), in_mode("output.normal")), setup=setup + "l")
        case(f"{m}.visual.swap_ends [o]", "o", all_of(at((0, 0), m), check(lambda b, a: tuple(selection(a)["anchor"]) == (0, 2), lambda b, a: f"selection {selection(a)}")), setup=setup + "ll")
        case(f"{m}.focus.up [esc]", "<esc>", in_mode("output.normal"), setup=setup)
        case(f"{m}.visual.char [v]", "v", in_mode("output.normal" if charwise else "output.visual"), setup=setup)
        case(f"{m}.visual.line [V]", "V", in_mode("output.visual_line" if charwise else "output.normal"), setup=setup)
    # Outputs are read-only: their editing keys are unbound, so the key is
    # not an action there (`:explain` reports it as not bound).
    for action, key in [("op.delete", "d"), ("edit.paste_after", "p"), ("edit.paste_before", "P"), ("edit.undo", "u"), ("edit.redo", "<c-r>")]:
        case(
            f"{m}.{action} [{key}] is not bound in read-only output",
            ":explain " + {"<c-r>": "ctrl+r"}.get(key, key) + "<cr>",
            all_of(
                unchanged_doc(),
                lambda b, a, s, al: ok("is not bound in this mode" in s, "explain does not say it is unbound:\n" + s),
            ),
            setup=setup,
        )

# ---------------------------------------------------------------- notebook

NBM = "notebook.normal"
sel = lambda index, part=None: check(
    lambda b, a: a["selected_cell"] == index and (part is None or a["selected_part"] == part) and mode(a) == NBM,
    lambda b, a: f"selected {a['selected_cell']} {a['selected_part']} in {mode(a)}, wanted {index} {part or ''}",
)
nb_row = lambda row: check(lambda b, a: a["nb_row"] == row and a["selected_cell"] == 0, lambda b, a: f"cell {a['selected_cell']} row {a['nb_row']}, wanted 0 row {row}")
for key in ["j", "<down>"]:
    case(f"{NBM}.motion.down [{key}]", key, nb_row(2))
for key in ["k", "<up>"]:
    case(f"{NBM}.motion.up [{key}]", key, nb_row(1), setup="j")
case(f"{NBM}.motion.fast_down [J]", "J", all_of(nb_row(7), sel(0, "Output")))
case(f"{NBM}.motion.fast_up [K]", "K", nb_row(1), setup="J")
case(f"{NBM}.motion.cell_next []]]", "]]", sel(1))
case(f"{NBM}.motion.cell_prev [[[]", "[[", sel(0), setup="]]")
case(f"{NBM}.motion.item_next [}}]", "}", sel(0, "Output"))
case(f"{NBM}.motion.item_prev [{{]", "{", sel(0, "Source"), setup="}")
for key in ["gg", "<home>"]:
    case(f"{NBM}.motion.top [{key}]", key, sel(0), setup="G")
for key in ["G", "<end>"]:
    case(f"{NBM}.motion.bottom [{key}]", key, sel(4))
for key in ["<c-d>", "<pgdn>"]:
    case(f"{NBM}.motion.half_page_down [{key}]", key, check(lambda b, a: a["selected_cell"] >= 5, lambda b, a: f"selected {a['selected_cell']}"), fixture=LONG)
for key in ["<c-u>", "<pgup>"]:
    case(f"{NBM}.motion.half_page_up [{key}]", key, check(lambda b, a: a["selected_cell"] <= 34, lambda b, a: f"selected {a['selected_cell']}"), setup="G", fixture=LONG)
for key in ["i", "l"]:
    case(f"{NBM}.cell.enter_normal [{key}]", key, at((0, 0), "cell.normal"))
    case(f"{NBM}.cell.enter_normal on an output [{key}]", key, in_mode("output.normal"), setup="}")
    case(f"{NBM}.cell.enter_normal on an empty cell [{key}]", key, in_mode("cell.insert"), setup="G")
case(f"{NBM}.cell.append_end [A]", "A", at((4, 11), "cell.insert"))
for key in ["a", "o"]:
    case(f"{NBM}.cell.insert_below [{key}]", key, check(lambda b, a: len(a["cells"]) == 6 and a["selected_cell"] == 1 and a["cells"][1]["source"] == "", lambda b, a: f"{len(a['cells'])} cells, selected {a['selected_cell']}"))
for key in ["b", "O"]:
    case(f"{NBM}.cell.insert_above [{key}]", key, check(lambda b, a: len(a["cells"]) == 6 and a["selected_cell"] == 0 and a["cells"][0]["source"] == "", lambda b, a: f"{len(a['cells'])} cells, selected {a['selected_cell']}"))
case(f"{NBM}.cell.run [enter]", "<cr>", check(lambda b, a: "42" in "".join(a["cells"][0]["outputs"]), "no 42"), fixture=KERNEL, kernel=True, wait_until=lambda s: "42" in "".join(s["cells"][0]["outputs"]))
case(f"{NBM}.output.cycle_view on a source runs it [space]", "<space>", check(lambda b, a: "42" in "".join(a["cells"][0]["outputs"]), "no 42"), fixture=KERNEL, kernel=True, wait_until=lambda s: "42" in "".join(s["cells"][0]["outputs"]))
case(f"{NBM}.output.cycle_view on an output [space]", "<space>", check(lambda b, a: a["cells"][0]["output_view"] != b["cells"][0]["output_view"], "the output view did not change"), setup="}")
case(f"{NBM}.output.next_tab [tab]", "<tab>", check(lambda b, a: a["cells"][3]["stream_tab"] != b["cells"][3]["stream_tab"], "the stream tab did not change"), setup="]]]]]]}")
case(f"{NBM}.cell.run_and_advance [r]", "r", all_of(sel(1), check(lambda b, a: "42" in "".join(a["cells"][0]["outputs"]), "no 42")), fixture=KERNEL, kernel=True, wait_until=lambda s: "42" in "".join(s["cells"][0]["outputs"]))
case(f"{NBM}.cell.run_all [X]", "X", check(lambda b, a: "42" in "".join(a["cells"][0]["outputs"]) and a["cells"][1]["execution_count"], "not every cell ran"), fixture=KERNEL, kernel=True, wait_until=lambda s: s["cells"][1]["execution_count"])
case(f"{NBM}.cell.toggle_kind [t]", "t", check(lambda b, a: a["cells"][0]["kind"] == "Markdown", lambda b, a: a["cells"][0]["kind"]))
case(f"{NBM}.cell.toggle_collapse [za]", "za", check(lambda b, a: a["cells"][0]["collapsed"], "not collapsed"))
case(f"{NBM}.cell.focus_output [zo]", "zo", in_mode("output.normal"))
case(f"{NBM}.cell.edit_external [<c-g>]", "<c-g>", check(lambda b, a: a["cells"][0]["source"].endswith("# edited"), "not edited"))
case(f"{NBM}.cell.clear_outputs [c]", "c", check(lambda b, a: a["cells"][0]["outputs"] == [], "outputs remain"))
case(f"{NBM}.cell.format [f]", "f", check(lambda b, a: a["cells"][0]["source"] == "x = 1", lambda b, a: repr(a["cells"][0]["source"])), fixture=FORMAT, quiet=1.0)
case(f"{NBM}.doc.save [w]", "w", check(lambda b, a: b["dirty"] and not a["dirty"], lambda b, a: f"dirty {b['dirty']} -> {a['dirty']}"), setup="t")
case(f"{NBM}.doc.save_force [W]", "W", check(lambda b, a: b["dirty"] and not a["dirty"], lambda b, a: f"dirty {b['dirty']} -> {a['dirty']}"), setup="t")
case(f"{NBM}.edit.paste_after [p]", "p", check(lambda b, a: len(a["cells"]) == 6 and a["cells"][1]["source"] == SOURCE0, lambda b, a: f"{len(a['cells'])} cells"), setup="yy")
case(f"{NBM}.edit.paste_before [P]", "P", check(lambda b, a: len(a["cells"]) == 6 and a["cells"][0]["source"] == SOURCE0 and a["selected_cell"] == 0, lambda b, a: f"{len(a['cells'])} cells"), setup="yy")
case(f"{NBM}.edit.undo [u]", "u", check(lambda b, a: len(a["cells"]) == 5, lambda b, a: f"{len(a['cells'])} cells"), setup="dd")
case(f"{NBM}.edit.redo [<c-r>]", "<c-r>", check(lambda b, a: len(a["cells"]) == 4, lambda b, a: f"{len(a['cells'])} cells"), setup="ddu")
case(f"{NBM}.op.delete [dd]", "dd", check(lambda b, a: len(a["cells"]) == 4 and a["cells"][0]["source"] == "y = 2", lambda b, a: f"{len(a['cells'])} cells"))
case(f"{NBM}.op.delete [dj]", "dj", check(lambda b, a: len(a["cells"]) == 3, lambda b, a: f"{len(a['cells'])} cells"))
case(f"{NBM}.op.delete on an output [dd]", "dd", check(lambda b, a: a["cells"][0]["outputs"] == [] and len(a["cells"]) == 5, "outputs not cleared"), setup="}")
case(f"{NBM}.op.yank [yy]", "yy", check(lambda b, a: a["yank_cells"] == 1, "nothing yanked"))
case(f"{NBM}.op.yank on an output [yy]", "yy", check(lambda b, a: "out three" in a["yank_text"], lambda b, a: repr(a["yank_text"])), setup="}")
case(f"{NBM}.focus.up [esc]", "<esc>", sel(0))
case(f"{NBM}.mark.set [m]", "ma", check(lambda b, a: "a" in a["marks"], "no mark a"), setup="]]")
case(f"{NBM}.mark.jump [']", "'a", sel(1), setup="]]ma]]]]")
case(f"{NBM}.search.next [n]", "n", sel(3), setup="/= <cr>")
case(f"{NBM}.search.prev [N]", "N", sel(1), setup="/= <cr>n")
case(f"{NBM}.search.open jumps [/]", "/z =<cr>", sel(3))
case(
    f"{NBM}.kernel.restart [R]",
    "R<wait:6000><cr>",
    check(lambda b, a: a["cells"][0]["execution_count"] == 1, lambda b, a: f"execution count {a['cells'][0]['execution_count']}"),
    setup="<cr><wait:1500><cr><wait:1500>",
    fixture=KERNEL,
    kernel=True,
    wait_until=lambda s: s["cells"][0]["execution_count"] == 1 and not s["kernel"]["busy"],
)
for key, pos in [("0", (1, 0)), ("^", (1, 4)), ("_", (1, 4)), ("$", (1, 21))]:
    case(f"{NBM}.line motion types [{key}]", key, at(pos, "cell.insert"), setup="j")
# Home and End keep their notebook meaning (first and last cell).

# ---------------------------------------------------------------- pdb / input()

PDB_READY = lambda s: s["pdb"] is not None


def pdb_input(text):
    return check(lambda b, a: a["pdb"] and a["pdb"]["input"] == text, lambda b, a: f"pdb {a['pdb']}")


def pdb_case(name, keys, chk, **extra):
    """Run the cell that calls input(), then test keys at its prompt."""
    case(f"pdb.input.{name}", keys, chk, setup="<cr>", fixture=PDB, kernel=True, setup_until=PDB_READY, **extra)


pdb_case("typing", "hi", pdb_input("hi"))
pdb_case("pdb.submit [enter]", "hi<cr>", check(lambda b, a: a["pdb"] and a["pdb"]["prompt"].startswith("second"), lambda b, a: f"pdb {a['pdb']}"), wait_until=lambda s: s["pdb"] and s["pdb"]["prompt"].startswith("second"))
for key in ["<bs>", "<c-h>"]:
    pdb_case(f"edit.backspace [{key}]", "hix" + key, pdb_input("hi"))
pdb_case("edit.delete_char [del]", "hi<left><del>", pdb_input("h"))
pdb_case("edit.delete_to_line_end [<c-k>]", "hi<left><c-k>", pdb_input("h"))
pdb_case("edit.delete_to_line_start [<c-u>]", "hi<c-u>", pdb_input(""))
for key, terminal in [("<c-w>", False), ("<a-bs>", False), ("<c-bs>", True)]:
    pdb_case(f"edit.delete_word_back [{key}]", "hi there" + key, pdb_input("hi "), terminal=terminal)
pdb_case("motion.left [left]", "hi<left>X", pdb_input("hXi"))
pdb_case("motion.right [right]", "hi<left><right>X", pdb_input("hiX"))
for key in ["<home>", "<c-a>"]:
    pdb_case(f"motion.line_start [{key}]", "hi" + key + "X", pdb_input("Xhi"))
for key in ["<end>", "<c-e>"]:
    pdb_case(f"motion.line_end [{key}]", "hi<left><left>" + key + "X", pdb_input("hiX"))
pdb_case("motion.word_back [a-b]", "hi there<a-b>X", pdb_input("hi Xthere"))
pdb_case("motion.word_next [a-f]", "hi there<home><a-f>X", pdb_input("hi Xthere"))
pdb_case("pdb.history_prev [up]", "hi<cr><wait:800><up>", pdb_input("hi"))
pdb_case("pdb.history_next [down]", "hi<cr><wait:800><up><down>", pdb_input(""))
for key in ["<c-j>", "<a-cr>"]:
    pdb_case(f"pdb.newline [{key}]", "a" + key + "b", pdb_input("a\nb"))
pdb_case("kernel.interrupt [<c-c>]", "<c-c>", check(lambda b, a: a["pdb"] is None, "the prompt is still open"), wait_until=lambda s: s["pdb"] is None)
pdb_case("app.quit [<c-q>]", "<c-q>", quits())
pdb_case("app.palette [<c-/>]", "<c-/>", modal("CommandPalette"))
pdb_case("app.settings [<c-,>]", "<c-,>", modal("Settings"), terminal=True)
pdb_case("ui.toggle_mouse_capture [<a-m>]", "<a-m>", check(lambda b, a: a["mouse_capture"] != b["mouse_capture"], "did not toggle"))
