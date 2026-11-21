#!/usr/bin/env python3
# scripts/coverage_report.py - Issue #363
# Enhanced error messages when crates are missing from config

import json
import sys
from pathlib import Path

def load_thresholds(config_path):
    """Load coverage thresholds from JSON config."""
    try:
        with open(config_path) as f:
            return json.load(f)
    except FileNotFoundError:
        print(f"ERROR: Configuration file not found: {config_path}")
        sys.exit(1)
    except json.JSONDecodeError as e:
        print(f"ERROR: Invalid JSON in {config_path}: {e}")
        sys.exit(1)

def get_workspace_crates():
    """Get list of crates from Cargo.toml workspace."""
    crates = []
    cargo_toml = Path("Cargo.toml")

    if not cargo_toml.exists():
        print("ERROR: Cargo.toml not found")
        sys.exit(1)

    with open(cargo_toml) as f:
        in_workspace = False
        for line in f:
            if "[workspace]" in line:
                in_workspace = True
            elif in_workspace and "members" in line:
                # Parse: members = ["crate1", "crate2", ...]
                members_str = line.split("=", 1)[1].strip()
                # Simple parsing for common formats
                members_str = members_str.replace("[", "").replace("]", "").replace('"', "")
                crates = [c.strip() for c in members_str.split(",") if c.strip()]
                break

    return crates

def check_coverage_config(config_path="coverage-thresholds.json"):
    """Check that all workspace crates have coverage thresholds configured."""
    thresholds = load_thresholds(config_path)
    crates = get_workspace_crates()

    missing_crates = []
    for crate in crates:
        if crate not in thresholds:
            missing_crates.append(crate)

    if missing_crates:
        print(f"ERROR: The following crates are missing from {config_path}:")
        for crate in missing_crates:
            print(f"  - {crate}")
        print(f"\nAdd coverage thresholds for these crates to {config_path}")
        print(f"Example entry:")
        print(f'  "{missing_crates[0]}": {{"line": 80.0, "branch": 70.0}}')
        sys.exit(1)

    print(f"✓ All {len(crates)} workspace crates have coverage thresholds configured")
    return 0

if __name__ == "__main__":
    sys.exit(check_coverage_config())
