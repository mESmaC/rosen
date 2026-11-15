"""Split the monolithic spec for #384, without changing its words.

Run from any directory: python .github/scripts/split_spec.py
To repeat on a newer unsplit base, pass --source <monolithic-SPEC.md>.
Use --join to print the original order with only whitespace changed.
Running again on an already split tree refreshes the indexes from that text.
The numbered proof table stays in SPEC.md: Markdown tables cannot soft-wrap.
No third-party packages, builds, tests or Lean execution are needed.
"""

import argparse
from pathlib import Path
import re


FEATURES = {
    1: "repository-import-and-git-mirror",
    2: "changes-snapshots-and-offline-work",
    3: "workspaces",
    4: "workspace-lifecycle-and-deletion",
    5: "automatic-merge-and-conflicts",
    6: "path-claims-and-edit-tracking",
    7: "review",
    8: "optimistic-landing",
    9: "events",
    10: "cli",
    11: "history-queries",
    12: "post-land-verification",
    13: "airc-channel-on-the-server",
    14: "network-access-and-enrollment",
}
SECTIONS = {
    "5": "5-done-means",
    "6": "6-out-of-scope",
    "8": "8-open-questions",
    "9": "9-velocity-changes",
    "proof": "proof-list",
}
BUSY = {2, 3, 8}
INDEX = "<!-- split-spec: index -->"
RULES = "<!-- split-spec: rules -->"
LIST_ITEM = re.compile(r"^( *)(?:- |\d+\. )")
SENTENCE = re.compile(r'''[.!?][\"'’”)]*(?:\*\*)?(?=\s+\S)''')
RULE_NAMES = {
    "rosen-change-new-on-change-creates-a-change": "New changes",
    "before-acknowledging-a-snapshot-including-an-unchanged-tree": "Snapshot repair",
    "snapshots-and-commits-belong-to-the-change-and": "Immutable snapshot history",
    "the-caller-first-flushes-a-final-snapshot-then": "Flush before applying a rebase",
    "if-the-files-on-disk-differ-from-that": "Refuse unstored edits",
    "a-server-rebase-never-writes-into-a-local": "Explicit local application",
    "correctness-tests-wait-for-the-snapshot-with-a": "Snapshot correctness and timing",
    "new-changes-and-warm-pools-use-the-current": "Workspace starting commit",
    "otherwise-a-parallel-plain-copy": "Parallel plain copy",
    "sparse-checkout-lets-a-workspace-hold-only-chosen": "Sparse checkout",
    "a-workspace-is-built-in-a-hidden-directory": "Atomic workspace publication",
    "the-hidden-directory-s-name-carries-the-building-process-s": "Crash staging cleanup",
    "a-block-clone-is-checked-by-hashing-one": "Block clone verification",
    "on-windows-a-symlink-in-the-tree-is": "Windows symlinks",
    "a-rename-works-only-within-one-volume-so": "Warm pool location",
    "prepare-commands-run-for-one-workspace-at-a": "Prepare scheduling",
    "a-checker-that-builds-a-change-s-merge-result": "Checker build trees",
    "after-a-land-the-daemon-runs-them-again": "Prepare after landing",
    "a-workspace-whose-prepare-failed-is-retried-when": "Prepare retries",
    "a-prepare-that-leaves-a-link-or-a": "Unsafe prepare output",
    "each-command-runs-through-the-platform-shell-cmd": "Prepare command execution",
    "a-run-has-a-class-most-important-first": "Run classes",
    "each-class-has-a-number-of-slots-and": "Run slots",
    "before-granting-any-new-turn-the-queue-also": "Memory reserve",
    "default-from-a-measured-ramp-2026-11-14-one-windows": "Measured slot defaults",
    "rosen-run-slots-prints-each-class-s-count-thread": "Machine queue settings",
    "commands-started-by-rosen-run-default-cargo-build-jobs-rust-test-threads": "Command thread defaults",
    "a-less-important-run-that-is-already-going": "Process priorities",
    "priority-alone-cannot-hold-a-run-forever-after": "Queue aging",
    "rosen-run-class-class-command-waits-for": "Run command and status",
    "on-windows-rosen-run-creates-its-command-launcher": "Windows process containment",
    "a-turn-passes-to-the-processes-it-runs": "Nested runs",
    "a-materialisation-failure-names-the-failing-admission-file": "Materialisation diagnostics",
    "machine-admission-is-shared-by-clients-and-pool": "Machine admission limits",
    "a-request-that-has-waited-5-s-says": "Admission wait diagnostics",
    "disk-admission-reserves-before-copying-the-most-logical": "Disk reservations",
    "one-process-per-machine-at-a-time-scans": "Workspace size scanning",
    "each-landing-attempt-reads-the-check-set-name-and": "Attempt configuration",
    "rosen-land-change-asks-the-server-to-land": "Land request",
    "every-change-in-the-requested-stack-has-no": "Clean merge and conflict markers",
    "in-before-land-mode-every-layer-also-needs-an": "Stack approval and receipt gates",
    "with-recheck-merge-a-valid-current-head-receipt": "Merge recheck mode",
    "with-recheck-reach-default-every-change-landed": "Reach recheck mode",
    "otherwise-in-reach-mode-with-a-clean-merge": "New receipt requirement",
    "in-reach-mode-reach-covers-all-stack-layers": "Stack reach",
    "conflict-the-change-gets-stored-conflict-regions": "Conflict routing",
    "the-ref-move-every-layer-s-landed-history-insertion-and": "Atomic land transaction",
    "if-the-compare-and-swap-loses-a-race-the-server": "Compare and swap retry",
    "a-requester-whose-connection-drops-mid-land-asks-again": "Repeat land requests",
    "a-stack-lands-in-one-request-rosen-land": "Atomic stack landing",
    "the-server-remembers-the-parent-tree-each-child": "Parent propagation",
    "a-legacy-edited-stack-with-no-recoverable-parent-tree": "Legacy stack basis",
    "its-owner-recovers-it-with-rosen-change-recover": "Stack basis recovery",
    "a-stacked-change-s-merge-result-step-1-measures": "Stack merge result",
    "the-requester-is-answered-once-the-transaction-that": "Land response",
    "each-landed-layer-has-its-own-pending-post-land": "Per layer recovery",
    "both-first-and-already-landed-answers-wait-for-every": "Restack before answering",
    "the-server-rebases-changes-stacked-on-the-landed": "Restack after landing",
    "the-server-probes-changes-whose-edit-regions-overlap": "Overlap conflict probes",
    "probe-targets-are-saved-before-releasing-the-landed": "Saved probe targets",
    "claim-only-overlaps-receive-the-landed-notice-through-the": "Claim overlap notices",
    "the-mirror-push-is-scheduled": "Schedule mirror push",
    "post-land-work-that-a-crash-or-a-failed": "Post land recovery",
}


def feature_path(number):
    path = f"spec/R{number}-{FEATURES[number]}"
    return path + ("/README.md" if number in BUSY else ".md")


def slug(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def sentence_lines(text):
    lines = []
    for line in text.splitlines():
        if line.startswith("#"):
            lines.append(line)
            continue
        item = LIST_ITEM.match(line)
        start = item.end() if item else len(line) - len(line.lstrip())
        continuation = " " * start
        code = list(re.finditer(r"(`+).*?\1", line))
        ends = [match.end() for match in SENTENCE.finditer(line, start)
                if not any(span.start() <= match.start() < span.end() for span in code)]
        position = 0
        for end in ends:
            lines.append((continuation if position else "") + line[position:end])
            position = end
            while position < len(line) and line[position].isspace():
                position += 1
        lines.append((continuation if position else "") + line[position:])
    return "\n".join(lines).rstrip() + "\n"


def rule_name(text):
    first = " ".join(text.split())
    first = re.sub(r"^(?:- |\d+\. )", "", first)
    bold = re.match(r"\*\*(.*?)\*\*", first)
    label = bold[1] if bold else " ".join(first.split()[:8])
    label = RULE_NAMES.get(slug(label), label)
    return label.rstrip(".:"), slug(label)


def split_feature(text, number):
    path = feature_path(number)
    if number not in BUSY:
        return {path: sentence_lines(text)}
    lines = text.splitlines(keepends=True)
    starts = [i for i, line in enumerate(lines) if LIST_ITEM.match(line)]
    output = {}
    links = []
    names = set()
    for begin, end in zip(starts, starts[1:] + [len(lines)]):
        block = "".join(lines[begin:end])
        indent = LIST_ITEM.match(lines[begin])[1]
        block = "\n".join(line[len(indent):] if line.startswith(indent) else line
                          for line in block.splitlines())
        label, name = rule_name(block)
        if name in names:
            raise ValueError(f"Duplicate rule name in R{number}: {name}")
        names.add(name)
        output[str(Path(path).parent / (name + ".md")).replace("\\", "/")] = sentence_lines(block)
        label = re.sub(r"[`<>\[\]]", "", label)
        step = re.match(r"(\d+)\. ", block)
        if step:
            label = f"Step {step[1]} · {label}"
        links.append(f"{indent}- [{label}]({name}.md)")
    intro = sentence_lines("".join(lines[:starts[0]]))
    output[path] = intro + "\n" + RULES + "\n\n" + "\n".join(links) + "\n"
    return output


def compact_root(text):
    lines = text.rstrip().splitlines()
    return "\n".join(line for i, line in enumerate(lines)
                     if line or not ((i and (lines[i - 1].startswith("#") or
                                             lines[i - 1] == "---")) or
                                     (i + 1 < len(lines) and lines[i + 1].startswith("#"))))


def index_link(heading, path):
    # Preserve the old GitHub heading anchor for existing SPEC.md fragment URLs.
    anchor = re.sub(r"[^\w\- ]", "", heading.lower()).replace(" ", "-")
    return f'<a id="{anchor}"></a>[{heading}]({path})'


def split_document(source):
    sections = re.split(r"(?m)(?=^## (?:[4-9]\. |Proof list$))", source)
    if len(sections) != 8:
        raise ValueError("Expected monolithic SPEC.md with sections 4–9 and Proof list")
    prefix, features, done, scope, proofs, questions, velocity, legacy = sections
    output = {}
    feature_blocks = re.split(r"(?m)(?=^### R\d+ · )", features)[1:]
    if len(feature_blocks) != 14:
        raise ValueError("Expected R1–R14 exactly once")
    links = []
    for number, block in enumerate(feature_blocks, 1):
        block = re.sub(r"\n---\s*$", "\n", block)
        output.update(split_feature(block, number))
        links.append(index_link(block.splitlines()[0][4:], feature_path(number)))
    extras = []
    for key, block in zip(SECTIONS, [done, scope, questions, velocity, legacy]):
        path = f"spec/{SECTIONS[key]}.md"
        output[path] = sentence_lines(block)
        extras.append(index_link(block.splitlines()[0][3:], path))
    table = [f"| Feature {INDEX} | Feature | Feature |", "|---|---|---|"]
    links.append("")
    table += ["| " + " | ".join(links[i:i + 3]) + " |" for i in range(0, len(links), 3)]
    output["SPEC.md"] = (compact_root(prefix) + "\n## 4. Features\n" +
                         "\n".join(table) + "\n\n" + " · ".join(extras) + "\n" +
                         compact_root(proofs) + "\n")
    return output


def join_feature(read, number):
    path = feature_path(number)
    text = read(path)
    if number not in BUSY:
        return text
    intro, links = text.split(RULES, 1)
    blocks = [intro]
    for indent, name in re.findall(r"(?m)^( *)- \[.*?\]\(([^)]+)\)$", links):
        block = read(str(Path(path).parent / name).replace("\\", "/"))
        blocks.append("\n".join(indent + line if line else line for line in block.splitlines()))
    return "\n".join(blocks)


def join_document(read):
    root = read("SPEC.md")
    prefix, rest = root.split("## 4. Features", 1)
    proof_list = "## 7. Proof list" + rest.split("## 7. Proof list", 1)[1]
    blocks = [prefix, "## 4. Features"]
    blocks += [join_feature(read, number) for number in FEATURES]
    blocks += ["---", read("spec/5-done-means.md"), read("spec/6-out-of-scope.md"),
               proof_list, read("spec/8-open-questions.md"), read("spec/9-velocity-changes.md"),
               read("spec/proof-list.md")]
    return "\n\n".join(blocks)


def indexed_rules(read):
    paths = set()
    for number in BUSY:
        path = feature_path(number)
        links = read(path).split(RULES, 1)[1]
        for name in re.findall(r"(?m)^ *- \[.*?\]\(([^)]+)\)$", links):
            paths.add(str(Path(path).parent / name).replace("\\", "/"))
    return paths


def update_agent_guidance(text):
    text = text.replace("SPEC.md defines what to build.",
                        "SPEC.md indexes the specification; read the linked spec/ files for the feature you change.")
    text = text.replace("Update SPEC.md when behaviour changes.",
                        "Update the relevant spec/ rule file when behaviour changes.")
    text = text.replace("the SPEC.md section it implements; name that section.",
                        "the spec/ section it implements; name that file and section.")
    if "- R2, R3 and R8 keep one rule per file;" not in text:
        text = text.replace("## Scope\n", "## Scope\n" +
                            "- R2, R3 and R8 keep one rule per file; add new rules as new files in their spec/ folder, with one sentence per line.\n" +
                            "- The spec move can be repeated with `python .github/scripts/split_spec.py --source <monolithic-SPEC.md>`; `--join` prints its original order.\n")
    return text


def update_references(root):
    paths = [root / "AGENTS.md", root / ".github/workflows/ci.yml",
             root / ".github/workflows/proofs.yml"]
    paths += list((root / "proofs").rglob("*.lean"))
    for path in paths:
        if ".lake" in path.parts:
            continue
        text = path.read_text(encoding="utf-8")
        text = text.replace("SPEC.md §2 Reach and R8 step 2", "SPEC.md §2 Reach and " +
                            feature_path(8) + " R8 step 2")
        text = text.replace("SPEC R8/R12", "SPEC R8 and " + feature_path(12) + " R12")
        text = text.replace(feature_path(8) + " R8/R12", feature_path(8) +
                            " R8 and " + feature_path(12) + " R12")
        text = re.sub(r"SPEC(?:\.md)? (?:§)?R(\d+)\b",
                      lambda match: feature_path(int(match[1])) + f" R{match[1]}", text)
        for number, name in SECTIONS.items():
            text = text.replace(f"SPEC.md §{number}", f"spec/{name}.md §{number}")
        text = re.sub(r"proof list in SPEC\.md(?! §7)", "proof list in SPEC.md §7", text)
        if path.name == "AGENTS.md":
            text = update_agent_guidance(text)
        if text != path.read_text(encoding="utf-8"):
            path.write_text(text, encoding="utf-8", newline="\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="a newer monolithic SPEC.md")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--join", action="store_true", help="print the rejoined spec")
    args = parser.parse_args()
    read = lambda path: (args.root / path).read_text(encoding="utf-8")
    if args.join:
        print(join_document(read), end="")
        return
    source = args.source.read_text(encoding="utf-8") if args.source else read("SPEC.md")
    previous = indexed_rules(read) if INDEX in read("SPEC.md") else set()
    if INDEX in source:
        source = join_document(read)
    output = split_document(source)
    # Refuse the migration before writing anything if it loses or changes words.
    if source.split() != join_document(output.__getitem__).split():
        raise ValueError("Rejoining the split spec changed the original text")
    for path, text in output.items():
        destination = args.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text, encoding="utf-8", newline="\n")
    # Only retire formerly indexed fragments; leave unindexed additions alone.
    for path in previous - output.keys():
        destination = (args.root / path).resolve()
        if not destination.is_relative_to((args.root / "spec").resolve()):
            raise ValueError(f"Indexed rule is outside spec/: {path}")
        destination.unlink()
    update_references(args.root)
    print(f"Moved spec into {len(output) - 1} files; SPEC.md has {len(output['SPEC.md'].splitlines())} lines.")


if __name__ == "__main__":
    main()
