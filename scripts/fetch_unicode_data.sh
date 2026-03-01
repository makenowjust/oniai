#!/bin/sh
# Download Unicode CaseFolding.txt for use by build.rs.
#
# Usage:
#   sh scripts/fetch_unicode_data.sh [UNICODE_VERSION]
#
# The file is saved to data/CaseFolding.txt and committed to the repository
# so that builds are reproducible without network access.
#
# The default Unicode version is 17.0.0.  Pass a version string as the first
# argument to fetch a different release, e.g.:
#   sh scripts/fetch_unicode_data.sh 16.0.0

set -eu

UNICODE_VERSION="${1:-17.0.0}"
URL="https://www.unicode.org/Public/${UNICODE_VERSION}/ucd/CaseFolding.txt"
DEST="data/CaseFolding.txt"

mkdir -p data

echo "Downloading Unicode ${UNICODE_VERSION} CaseFolding.txt ..."
curl -fsSL "$URL" -o "$DEST"
echo "Saved to ${DEST}"
