#!/usr/bin/env bash
set -euo pipefail

export CARGO_TERM_COLOR=always
export CLICOLOR=1
export RUST_BACKTRACE=1

: "${GIT_BRANCH:?GIT_BRANCH is required}"
: "${GIT_COMMIT_NAME:?GIT_COMMIT_NAME is required}"
: "${GIT_COMMIT_EMAIL:?GIT_COMMIT_EMAIL is required}"
: "${GIT_PRIVATE_KEY:?GIT_PRIVATE_KEY is required}"

git config user.name "${GIT_COMMIT_NAME}"
git config user.email "${GIT_COMMIT_EMAIL}"

mkdir -p "${HOME}/.ssh"
chmod 700 "${HOME}/.ssh"
printf '%s\n' "${GIT_PRIVATE_KEY}" > "${HOME}/.ssh/id_ed25519"
chmod 600 "${HOME}/.ssh/id_ed25519"

cat > "${HOME}/.ssh/config" <<'EOF'
Host github.com
  IdentityFile ~/.ssh/id_ed25519
  StrictHostKeyChecking accept-new
EOF
chmod 600 "${HOME}/.ssh/config"

manifest_pathspec=('Cargo.lock' ':(glob)**/Cargo.toml')
had_failed_upgrades=0
declare -a successful_upgrades=()
declare -a no_update_packages=()
declare -a failed_upgrade_packages=()
declare -a failed_check_packages=()
declare -a manual_action_packages=()
current_attempt_summary=""

run_checks() {
  local package_name="${1:?package name is required}"
  local check_name
  local log_file

  for check_name in test clippy fmt; do
    log_file="$(mktemp)"

    case "${check_name}" in
      test)
        if cargo test --workspace --quiet >"${log_file}" 2>&1; then
          rm -f "${log_file}"
          continue
        fi
        ;;
      clippy)
        if cargo clippy --all-targets --all-features --quiet -- -D warnings >"${log_file}" 2>&1; then
          rm -f "${log_file}"
          continue
        fi
        ;;
      fmt)
        if cargo fmt --all -- --check >"${log_file}" 2>&1; then
          rm -f "${log_file}"
          continue
        fi
        ;;
    esac

    echo "Check '${check_name}' failed for ${package_name}. Last 40 log lines:"
    tail -n 40 "${log_file}"
    rm -f "${log_file}"
    return 1
  done
}

extract_relevant_upgrade_lines() {
  local output="${1:-}"

  awk '
    /^name[[:space:]]+old req/ { print; next }
    /^====/ { print; next }
    /^  [A-Za-z0-9_.+-]+[[:space:]]/ { print; next }
    /Locking [0-9]+ package/ { print; next }
    /Updating [^[:space:]]+ v[^[:space:]]+ -> v[^[:space:]]+/ { print; next }
    /Downgrading [^[:space:]]+ v[^[:space:]]+ -> v[^[:space:]]+/ { print; next }
    /Adding [^[:space:]]+ v[^[:space:]]+/ { print; next }
    /Removing [^[:space:]]+ v[^[:space:]]+/ { print; next }
    /latest:/ { print; next }
    /excluded:/ { print; next }
    /is ambiguous/ { print; next }
    /^help: re-run this command/ { print; next }
    /^  [^[:space:]]+@/ { print; next }
  ' <<<"${output}"
}

print_relevant_upgrade_lines() {
  local output="${1:-}"

  extract_relevant_upgrade_lines "${output}"
}

append_attempt_summary() {
  local phase="${1:?phase is required}"
  local output="${2:-}"
  local summary_lines

  summary_lines="$(
    extract_relevant_upgrade_lines "${output}" \
      | grep -E 'Updating |Downgrading |Adding |Removing |is ambiguous|^[[:space:]]+[^[:space:]]+@' || true
  )"

  if [[ -z "${summary_lines}" ]]; then
    summary_lines="$(
      extract_relevant_upgrade_lines "${output}" \
      | sed '/^$/d' \
      | paste -sd ' | ' -
    )"
  else
    summary_lines="$(printf '%s\n' "${summary_lines}" | sed '/^$/d' | paste -sd ' | ' -)"
  fi

  if [[ -z "${summary_lines}" ]]; then
    return 0
  fi

  if [[ -n "${current_attempt_summary}" ]]; then
    current_attempt_summary+=" ; "
  fi

  current_attempt_summary+="${phase}: ${summary_lines}"
}

run_cargo_upgrade() {
  local package_name="${1:?package name is required}"
  local upgrade_mode="${2:?upgrade mode is required}"
  local output
  local -a cmd=(cargo upgrade --package "${package_name}" --quiet)

  case "${upgrade_mode}" in
    incompatible)
      cmd+=(--incompatible allow)
      ;;
    pinned)
      cmd+=(--pinned allow)
      ;;
    *)
      echo "Unknown cargo upgrade mode: ${upgrade_mode}" >&2
      return 1
      ;;
  esac

  if output="$("${cmd[@]}" 2>&1)"; then
    append_attempt_summary "upgrade" "${output}"
    print_relevant_upgrade_lines "${output}"
    return 0
  fi

  append_attempt_summary "upgrade" "${output}"
  echo "cargo upgrade failed for ${package_name}. Last 40 log lines:"
  tail -n 40 <<<"${output}"
  return 1
}

update_lockfile_for_package() {
  local package_name="${1:?package name is required}"
  local update_output
  local package_spec
  local -a ambiguous_specs=()

  if update_output="$(cargo update "${package_name}" --recursive 2>&1)"; then
    append_attempt_summary "update" "${update_output}"
    print_relevant_upgrade_lines "${update_output}"
    return 0
  fi

  append_attempt_summary "update" "${update_output}"
  print_relevant_upgrade_lines "${update_output}"

  if grep -Fq "is ambiguous" <<<"${update_output}"; then
    mapfile -t ambiguous_specs < <(awk '/^  [^[:space:]]+@/ { print $1 }' <<<"${update_output}")

    if ((${#ambiguous_specs[@]} > 0)); then
      echo "Updating ambiguous lockfile entries for ${package_name}: ${ambiguous_specs[*]}"
      for package_spec in "${ambiguous_specs[@]}"; do
        if ! update_output="$(cargo update "${package_spec}" --recursive 2>&1)"; then
          append_attempt_summary "update ${package_spec}" "${update_output}"
          echo "cargo update failed for ${package_spec}. Last 40 log lines:"
          tail -n 40 <<<"${update_output}"
          return 1
        fi
        append_attempt_summary "update ${package_spec}" "${update_output}"
        print_relevant_upgrade_lines "${update_output}"
      done
      return 0
    fi
  fi

  echo "cargo update failed for ${package_name}. Last 40 log lines:"
  tail -n 40 <<<"${update_output}"
  return 1
}

has_manifest_changes() {
  ! git diff --quiet -- "${manifest_pathspec[@]}"
}

stage_manifest_changes() {
  git add --update -- "${manifest_pathspec[@]}"
}

reset_repo_state() {
  git reset --hard --quiet HEAD
}

commit_and_push() {
  local message="${1:?commit message is required}"

  echo "Re-validating before commit and push"
  run_checks "pre-push verification"
  stage_manifest_changes
  git commit -m "${message}"
  git push --no-verify origin "HEAD:${GIT_BRANCH}"
}

record_result() {
  local result_kind="${1:?result kind is required}"
  local package_name="${2:?package name is required}"
  local detail_suffix=""

  if [[ -n "${current_attempt_summary}" ]]; then
    detail_suffix=" (${current_attempt_summary})"
  fi

  case "${result_kind}" in
    success)
      successful_upgrades+=("${package_name}")
      ;;
    no-update)
      no_update_packages+=("${package_name}")
      ;;
    failed-upgrade)
      failed_upgrade_packages+=("${package_name}")
      manual_action_packages+=("${package_name}: cargo upgrade/update step failed${detail_suffix}")
      ;;
    failed-checks)
      failed_check_packages+=("${package_name}")
      manual_action_packages+=("${package_name}: checks failed after dependency changes${detail_suffix}")
      ;;
  esac
}

print_summary() {
  echo ""
  echo "-------"
  echo "Summary"
  echo "-------"

  if ((${#successful_upgrades[@]} > 0)); then
    echo "Upgraded and pushed (${#successful_upgrades[@]}): ${successful_upgrades[*]}"
  fi

  if ((${#no_update_packages[@]} > 0)); then
    echo "No update available (${#no_update_packages[@]}): ${no_update_packages[*]}"
  fi

  if ((${#failed_upgrade_packages[@]} > 0)); then
    echo "Upgrade step failed (${#failed_upgrade_packages[@]}): ${failed_upgrade_packages[*]}"
  fi

  if ((${#failed_check_packages[@]} > 0)); then
    echo "Checks failed and reverted (${#failed_check_packages[@]}): ${failed_check_packages[*]}"
  fi

  if ((${#manual_action_packages[@]} > 0)); then
    echo ""
    echo "Action items:"
    printf '  - %s\n' "${manual_action_packages[@]}"
  fi
}

ensure_repo_is_clean() {
  local status_output

  status_output="1003 960 1001 1002 1003git status --porcelain)"
  if [[ -z "" ]]; then
    return 0
  fi

  echo "Repository has uncommitted changes. Commit or stash them before running dependency upgrades."
  git status --short
  exit 1
}

ensure_baseline_is_green() {
  echo "Validating current HEAD before attempting dependency upgrades"
  if run_checks "baseline HEAD"; then
    return 0
  fi

  echo "Current HEAD is already failing checks. Resolve or revert existing breakage before running dependency upgrades."
  exit 1
}

if ! command -v cargo-upgrade >/dev/null 2>&1; then
  cargo install cargo-edit --locked
fi

rustup component add rustfmt clippy
ensure_repo_is_clean
ensure_baseline_is_green

mapfile -t normal_packages < <(
  python3 - <<'PY_DEPS'
from pathlib import Path
import tomllib


def iter_dependency_sections(document):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        section = document.get(key)
        if isinstance(section, dict):
            yield section

    workspace = document.get("workspace")
    if isinstance(workspace, dict):
        section = workspace.get("dependencies")
        if isinstance(section, dict):
            yield section

    targets = document.get("target")
    if isinstance(targets, dict):
        for target in targets.values():
            if not isinstance(target, dict):
                continue
            for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                section = target.get(key)
                if isinstance(section, dict):
                    yield section


packages = set()
for manifest_path in sorted(Path(".").rglob("Cargo.toml")):
    if "target" in manifest_path.parts:
        continue

    document = tomllib.loads(manifest_path.read_text())
    for section in iter_dependency_sections(document):
        for dependency_name, dependency_spec in section.items():
            package_name = dependency_name
            version = None

            if isinstance(dependency_spec, str):
                version = dependency_spec
            elif isinstance(dependency_spec, dict):
                if dependency_spec.get("workspace") is True:
                    continue
                if any(key in dependency_spec for key in ("git", "path")):
                    continue
                version = dependency_spec.get("version")
                package_name = dependency_spec.get("package", dependency_name)

            if isinstance(version, str) and not version.startswith("="):
                packages.add(package_name)

for package_name in sorted(packages):
    print(package_name)
PY_DEPS
)

mapfile -t pinned_packages < <(
  python3 - <<'PY_DEPS'
from pathlib import Path
import tomllib


def iter_dependency_sections(document):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        section = document.get(key)
        if isinstance(section, dict):
            yield section

    workspace = document.get("workspace")
    if isinstance(workspace, dict):
        section = workspace.get("dependencies")
        if isinstance(section, dict):
            yield section

    targets = document.get("target")
    if isinstance(targets, dict):
        for target in targets.values():
            if not isinstance(target, dict):
                continue
            for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                section = target.get(key)
                if isinstance(section, dict):
                    yield section


packages = set()
for manifest_path in sorted(Path(".").rglob("Cargo.toml")):
    if "target" in manifest_path.parts:
        continue

    document = tomllib.loads(manifest_path.read_text())
    for section in iter_dependency_sections(document):
        for dependency_name, dependency_spec in section.items():
            package_name = dependency_name
            version = None

            if isinstance(dependency_spec, str):
                version = dependency_spec
            elif isinstance(dependency_spec, dict):
                if dependency_spec.get("workspace") is True:
                    continue
                if any(key in dependency_spec for key in ("git", "path")):
                    continue
                version = dependency_spec.get("version")
                package_name = dependency_spec.get("package", dependency_name)

            if isinstance(version, str) and version.startswith("="):
                packages.add(package_name)

for package_name in sorted(packages):
    print(package_name)
PY_DEPS
)

echo ""
echo "-----------------------------------------"
echo "Attempting one-by-one dependency upgrades"
echo "-----------------------------------------"
echo ""
if ((${#normal_packages[@]} == 0)); then
  echo "No non-pinned registry dependencies were found."
else
  for package_name in "${normal_packages[@]}"; do
    reset_repo_state
    current_attempt_summary=""
    echo "Attempting upgrade for ${package_name}"
    if ! run_cargo_upgrade "${package_name}" incompatible; then
      echo "Upgrade step failed for ${package_name}; reverting."
      reset_repo_state
      had_failed_upgrades=1
      record_result failed-upgrade "${package_name}"
      continue
    fi

    if ! update_lockfile_for_package "${package_name}"; then
      echo "Lockfile update failed for ${package_name}; reverting."
      reset_repo_state
      had_failed_upgrades=1
      record_result failed-upgrade "${package_name}"
      continue
    fi

    if ! has_manifest_changes; then
      echo "No update available for ${package_name}"
      record_result no-update "${package_name}"
      continue
    fi

    if run_checks "${package_name}"; then
      commit_and_push "Chore(deps): upgrade ${package_name}"
      record_result success "${package_name}"
      continue
    fi

    echo "Checks failed for upgrade ${package_name}; reverting."
    reset_repo_state
    had_failed_upgrades=1
    record_result failed-checks "${package_name}"
  done
fi

echo ""
echo "---------------------------------"
echo "Attempting to update pinned items"
echo "---------------------------------"
echo ""
if ((${#pinned_packages[@]} == 0)); then
  echo "No pinned dependencies were found."
else
  for package_name in "${pinned_packages[@]}"; do
    reset_repo_state
    current_attempt_summary=""
    echo "Attempting pinned upgrade for ${package_name}"
    if ! run_cargo_upgrade "${package_name}" pinned; then
      echo "Pinned upgrade step failed for ${package_name}; reverting."
      reset_repo_state
      had_failed_upgrades=1
      record_result failed-upgrade "${package_name}"
      continue
    fi

    if ! update_lockfile_for_package "${package_name}"; then
      echo "Pinned lockfile update failed for ${package_name}; reverting."
      reset_repo_state
      had_failed_upgrades=1
      record_result failed-upgrade "${package_name}"
      continue
    fi

    if ! has_manifest_changes; then
      echo "No pinned update available for ${package_name}"
      record_result no-update "${package_name}"
      continue
    fi

    if run_checks "${package_name}"; then
      commit_and_push "Chore(deps): upgrade pinned ${package_name}"
      record_result success "${package_name}"
      continue
    fi

    echo "Checks failed for pinned upgrade ${package_name}; reverting."
    reset_repo_state
    had_failed_upgrades=1
    record_result failed-checks "${package_name}"
  done
fi

print_summary

if ((had_failed_upgrades)); then
  echo "One or more dependency upgrades failed checks and were reverted."
  exit 1
fi
