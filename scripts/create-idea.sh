#!/usr/bin/env sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

worktree_label() {
  if top_level=$(git rev-parse --show-toplevel 2>/dev/null); then
    common_dir=$(git -C "$top_level" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || git -C "$top_level" rev-parse --git-common-dir)
    case "$common_dir" in
      /*) common_abs=$common_dir ;;
      *) common_abs=$top_level/$common_dir ;;
    esac
    repo_name=$(basename "$(dirname "$common_abs")")
    worktree_name=$(basename "$top_level")
    if [ "$repo_name" = "$worktree_name" ]; then
      printf '%s\n' "$repo_name"
    else
      printf '%s@%s\n' "$repo_name" "$worktree_name"
    fi
  else
    basename "$project_dir"
  fi
}

# The interpreter this worktree owns, spelled the way the IDE registers it: the
# project directory with $HOME collapsed to a tilde, suffixed /.venv. Binding by
# path rather than by a label keeps the SDK worktree-local by construction --
# there is no name for a sibling worktree's interpreter to collide with.
venv_path() {
  case "$project_dir" in
    "$HOME"/*) printf '~%s/.venv\n' "${project_dir#"$HOME"}" ;;
    *) printf '%s/.venv\n' "$project_dir" ;;
  esac
}

label=$(worktree_label)
venv=$(venv_path)
rm -rf .idea.tmp
mkdir -p .idea.tmp
cp -R resources/idea-template/. .idea.tmp/
find .idea.tmp -type f -print | while IFS= read -r file; do
  tmp=$file.tmp
  awk -v worktree="$label" -v venv="$venv" '{
    gsub(/@@WORKTREE@@/, worktree)
    gsub(/@@VENV@@/, venv)
    print
  }' "$file" > "$tmp"
  mv "$tmp" "$file"
done
rm -rf .idea
mv .idea.tmp .idea
