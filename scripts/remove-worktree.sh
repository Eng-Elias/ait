#!/bin/sh
# Ticket-worktree teardown for runbook rounds — the mechanized form of
# docs/agents/subagents.md's Teardown bullet. Usage:
#
#   scripts/remove-worktree.sh <worktree-path> [branch]
#
# Kills e2e children still running from the worktree, removes the
# worktree *and* its target dir, then deletes the branch (GitHub
# already deleted the remote side on merge). Run it from outside the
# worktree being removed — typically the integration worktree or the
# main checkout.
#
# Why a script rather than the steps by hand: `pkill -f <worktree>`
# matches the CALLER's own command line whenever the worktree path
# appears anywhere on it — a chained `git worktree remove <path>`
# repeats the literal name, and even a bracket-regex pattern still
# matches the follow-up command's argument (R16 hit this 3x across
# mergers and the parent). And an argv pattern can't see Xvfb at
# all — its command line carries no worktree path — which is how
# ~970 stale Xvfb survived R15's removed worktrees. Matching on
# /proc/<pid>/cwd under the worktree catches ait-gui, Xvfb and
# leftover test binaries alike: every e2e child inherits its
# spawner's cwd, and this script never sits inside the worktree it
# removes, so it cannot self-match.
set -eu

wt="$1"
branch="${2:-$(basename "$wt")}"

[ -d "$wt" ] || { echo "remove-worktree: $wt is not a directory" >&2; exit 1; }
wt="$(cd "$wt" && pwd)"
case "$(pwd)" in
    "$wt"|"$wt"/*)
        echo "remove-worktree: cd out of $wt first" >&2
        exit 1
        ;;
esac

# Orphaned e2e children hold the worktree's soon-to-be-deleted inodes
# on near-full volumes — kill them before the directory goes.
killed=0
for link in /proc/[0-9]*/cwd; do
    pid=${link%/cwd}
    pid=${pid#/proc/}
    [ "$pid" = "$$" ] && continue
    case "$(readlink "$link" 2>/dev/null)" in
        "$wt"|"$wt"/*)
            kill "$pid" 2>/dev/null && killed=$((killed + 1))
            ;;
    esac
done
if [ "$killed" -gt 0 ]; then
    echo "remove-worktree: killed $killed process(es) running under $wt"
    sleep 1
fi

repo="$(dirname "$0")/.."
git -C "$repo" worktree remove --force "$wt"
git -C "$repo" branch -D "$branch" 2>/dev/null || true
