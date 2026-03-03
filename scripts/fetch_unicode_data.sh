#!/bin/sh
# Download Unicode data files used by build.rs.
#
# Usage:
#   sh scripts/fetch_unicode_data.sh [UNICODE_VERSION]
#
# Files saved to the data/ directory and committed to the repository
# so that builds are reproducible without network access.
#
# The default Unicode version is 17.0.0.  Pass a version string as the first
# argument to fetch a different release, e.g.:
#   sh scripts/fetch_unicode_data.sh 16.0.0

set -eu

UNICODE_VERSION="${1:-17.0.0}"
BASE="https://www.unicode.org/Public/${UNICODE_VERSION}/ucd"

fetch() {
    local url="$1"
    local dest="$2"
    mkdir -p "$(dirname "$dest")"
    echo "Downloading $url ..."
    curl -fsSL "$url" -o "$dest"
    echo "Saved to ${dest}"
}

fetch "${BASE}/CaseFolding.txt"                        "data/CaseFolding.txt"
fetch "${BASE}/extracted/DerivedGeneralCategory.txt"   "data/extracted/DerivedGeneralCategory.txt"
fetch "${BASE}/DerivedCoreProperties.txt"              "data/DerivedCoreProperties.txt"
fetch "${BASE}/PropList.txt"                           "data/PropList.txt"
