---
description: Release a new salash version - branch, version bump, local tag, test on macOS and arm64/x86_64 Linux, push the tag so CI builds the binaries, publish
argument-hint: <version, e.g. 0.0.2>
---

# Release salash v$ARGUMENTS

Run the release step by step. Stop and report to the user whenever a check fails. Don't work around it.

## What ships

`.github/workflows/release.yml` runs on any pushed `v*` tag. It creates a **draft** GitHub release and uploads:

| Target | Runner | Archive |
|---|---|---|
| `aarch64-apple-darwin` | `macos-latest` | `salash-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | `salash-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |
| `x86_64-unknown-linux-musl` | `ubuntu-latest` | `salash-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |

Each archive has `salash`, `LICENSE` and `README.md`, plus a matching `.sha256` file. That makes 6 assets.

**Not shipped, not tested:**
- **Intel macOS.**

Don't add targets as part of a release.

## Guardrails

- **Pushing the tag is the point of no return.** Before that, all testing is on local builds. Ask the user before pushing the tag, and again before publishing the release.
- Never force-push `main`. Never reuse a version that was published. If a release goes wrong, bump the patch version.
- Every salash run during testing uses an **isolated data dir**. Claude tabs inherit `SALASH_DATA`, `SALASH_TAB` and `SALASH_ISSUE` from the user's real salash. Running salash without overriding them touches the real instance.
- **Never skip the real-Claude suspend/resume test.** Resume must not break.

Use this isolated env in every shell that runs a salash binary, on macOS and inside the VMs:

```sh
export T=$(mktemp -d)
export SALASH_DATA=$T/data SALASH_CONFIG=$T/config.toml HOME_TEST=$T/home
unset SALASH_TAB SALASH_ISSUE
mkdir -p $T/vault $T/work $HOME_TEST
printf 'vault = "%s/vault"\n' "$T" > $SALASH_CONFIG
cd $T
```

The `cd $T` matters. salash commands use the nearest `.salash` above the current directory, the way git finds `.git`, and that beats `SALASH_DATA`. The release worktree sits inside the user's salash folder, so running from it would use the real `.salash`. Set `B` to an absolute path before the `cd`.

## 0. Inputs and preflight

1. Set `VERSION=$ARGUMENTS`.
   - If it's empty, take `git describe --tags --abbrev=0`, propose the next patch version and ask the user.
   - `VERSION` must be plain semver without a `v`. It must be greater than the last tag (`PREV`).
2. In the main checkout:
   - `git fetch origin --tags`
   - the checkout is on `main`, `git status --porcelain` is empty except untracked files, and `git rev-parse main` equals `git rev-parse origin/main`
3. CI must be green on that commit for ubuntu and macos. Find the run with `gh run list --workflow ci.yml --branch main -L1 --json databaseId,headSha`; the `headSha` must be `main`'s HEAD. Then check the jobs with `gh run view <id> --json jobs -q '.jobs[]|"\(.name): \(.conclusion)"'`.
4. `git tag -l vX.Y.Z` and `gh release view vX.Y.Z` both show nothing, so the tag and release don't exist yet.

## 1. Release branch

```sh
git worktree add .claude/worktrees/release-vX.Y.Z -b release/vX.Y.Z main
cd .claude/worktrees/release-vX.Y.Z
```

1. Set `version = "X.Y.Z"` in `Cargo.toml`. Run `cargo build`, which updates `Cargo.lock`. Check that `git diff` only touches those two files.
2. Run `cargo clippy --all-targets -- -D warnings` and `cargo test`.
3. Check README's **Install** section still matches the shipped platforms in the table above.
4. Draft the release notes into the scratchpad as `release-notes-vX.Y.Z.md`:
   - start from `git log vPREV..main --merges --oneline` (PR titles) plus notable direct commits
   - group them under Features / Fixes / Other
   - show the user the draft
5. Commit:
   ```sh
   git commit -am "Release vX.Y.Z"
   ```

## 2. Local tag (do not push)

```sh
git tag -a vX.Y.Z -m "salash X.Y.Z"
```

If a fix is needed later, commit it on the release branch and move the tag with `git tag -fa vX.Y.Z -m "salash X.Y.Z"`. This is fine as long as the tag hasn't been pushed.

## 3. macOS test (Apple silicon, native)

1. In the release worktree, `cargo build --release`, then set `B=$PWD/target/release/salash`. Then set up the isolated env, which ends with `cd $T`.
2. Automated checks, run from `$T`. They must all pass:
   - `$B --version` prints `salash X.Y.Z`
   - `$B --help`
   - `$B paths` shows config, data and notes under `$T`
   - `$B new release smoke test` then `$B list` shows the issue, and a note appears under `$T/vault`
   - `HOME=$HOME_TEST $B install-hooks` writes `$HOME_TEST/.claude/settings.json`
   - the plugin gets written: start a headless server with `$B server $T/work &`, wait for `$T/work/.salash/plugin/.claude-plugin/plugin.json`, then kill the server. The manifest has `"version": "X.Y.Z"`, and `$T/work/.salash/plugin/skills/new-salash-tab/SKILL.md` exists.
3. Manual checklist. Give it to the user. They run `$B $T/work` in a separate terminal with the same env vars. Get an explicit confirmation for each item:
   - [ ] It launches. Issues are on the left. Creating an issue writes its Obsidian note.
   - [ ] Open a Claude tab. It works, and the statuses show: working (yellow half-circle) and needs-you (triangle).
   - [ ] Shell and notes tabs open.
   - [ ] Resize the terminal. Scroll back in a pane.
   - [ ] Detach, then `$B -a $T/work` reattaches with the tabs intact.
   - [ ] **Suspend/resume with real Claude.** Quit salash fully. Relaunch. The Claude tab resumes the **same** session, with the earlier conversation visible.

## 4. Linux tests (arm64 + x86_64) in Lima

Use two dedicated Lima VMs. Never use the user's other VMs (`default`, `petarda-dev`).

| VM | Type | Arch | Musl target |
|---|---|---|---|
| `salash-arm` | `vz` (native, fast) | `aarch64` | `aarch64-unknown-linux-musl` |
| `salash-x86` | `qemu` (emulated, slow) | `x86_64` | `x86_64-unknown-linux-musl` |

1. Create the VMs if `limactl list` doesn't have them:
   ```sh
   limactl create --tty=false --name=salash-arm --vm-type=vz   --arch=aarch64 --cpus=4 --memory=4 template:ubuntu-lts
   limactl create --tty=false --name=salash-x86 --vm-type=qemu --arch=x86_64  --cpus=4 --memory=4 template:ubuntu-lts
   ```
   Then `limactl start salash-arm` and `limactl start salash-x86`. Starting the x86 VM takes minutes.
   If qemu is missing, ask the user to `brew install qemu`.
2. Provision each VM. Every step is idempotent, so it's safe to re-run:
   ```sh
   limactl shell <vm> sudo apt-get update
   limactl shell <vm> sudo apt-get install -y build-essential musl-tools git curl tmux file
   limactl shell <vm> sh -c 'command -v cargo || curl -sSf https://sh.rustup.rs | sh -s -- -y'
   limactl shell <vm> sh -c '. ~/.cargo/env && rustup update stable && rustup target add $(uname -m)-unknown-linux-musl'
   ```
3. Copy in the **exact tagged source**. Build in the VM's own filesystem, not under the host mount, so nothing is written into the host's `target/`:
   ```sh
   limactl shell <vm> sh -c 'rm -rf ~/salash-src && mkdir ~/salash-src'
   git archive vX.Y.Z | limactl shell <vm> sh -c "tar -x -C ~/salash-src"
   ```
4. Build and run the automated checks in each VM, with the isolated env set up inside the VM:
   - `cargo test`
   - `cargo build --release --target $(uname -m)-unknown-linux-musl`
   - `file target/<target>/release/salash` says **statically linked**
   - the same automated checks as macOS step 3.2: `--version`, `--help`, `paths`, `new`/`list`, `install-hooks`, plugin
5. Manual checklist, the same one as macOS step 3.3. The user runs `limactl shell salash-arm` (then `salash-x86`), sets the isolated env and runs the built binary.
   - Real Claude inside a VM needs `curl -fsSL https://claude.ai/install.sh | bash` and a `claude` login. Offer that.
   - On `salash-x86`, the user can skip the Claude-tab items because qemu is slow. On `salash-arm`, the suspend/resume item is required.
6. If anything fails, fix it on `release/vX.Y.Z`, move the tag (step 2), and re-run the affected tests.
7. When done: `limactl stop salash-arm salash-x86`. Keep the VMs for the next release.

## 5. Land the release commit

1. Push the release branch:
   ```sh
   git push -u origin release/vX.Y.Z
   ```
2. Open a PR into `main` with `gh pr create`. The body has:
   - the release notes
   - the test matrix results (macOS arm64, Linux arm64, Linux x86_64, each automated and manual)
   - a note that the tag will be pushed after the merge
3. The **user** merges the PR with a **merge commit, not squash**, so the tagged commit stays in `main`'s history. Don't merge it yourself.
4. After the merge: `git fetch origin`, then check that `git merge-base --is-ancestor vX.Y.Z origin/main` succeeds.
   - If it was squashed anyway, check that `git diff vX.Y.Z origin/main` is empty.
   - Then re-tag `origin/main` with `git tag -fa`.

## 6. Push the tag → CI builds the binaries

1. Ask the user for confirmation, then run `git push origin vX.Y.Z`.
2. Find the run with `gh run list --workflow release.yml -L1`, then `gh run watch <id> --exit-status`.
3. Check the assets with `gh release view vX.Y.Z --json isDraft,assets`. It must be a draft with exactly the 6 assets from the table above.
4. Verify the assets in the scratchpad:
   ```sh
   gh release download vX.Y.Z -D <scratchpad>/release-vX.Y.Z
   ```
   - check every `.sha256`
   - run `--version` on the macOS binary locally
   - copy each Linux tarball into its VM (`limactl copy`) and run `--version` there
   - start the VMs first, and stop them afterwards

## 7. Publish

1. Show the user the final release notes. After they confirm:
   ```sh
   gh release edit vX.Y.Z --notes-file <scratchpad>/release-notes-vX.Y.Z.md --draft=false
   ```
2. Check `gh release view vX.Y.Z` shows it as Latest.
3. Clean up:
   ```sh
   git worktree remove .claude/worktrees/release-vX.Y.Z
   git branch -d release/vX.Y.Z
   ```
   Delete the remote branch only if the user wants.

## Rollback

**Before the tag is pushed:** delete the local tag and branch. Nothing public happened.

**After the tag is pushed:**
1. Delete the release and the tag:
   ```sh
   gh release delete vX.Y.Z --yes
   git push --delete origin vX.Y.Z
   git tag -d vX.Y.Z
   ```
2. Fix the problem and release **X.Y.(Z+1)**. If vX.Y.Z was ever published (not just a draft), don't push it again.
