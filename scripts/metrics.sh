#!/usr/bin/env bash
# scripts/metrics.sh — one screen of the numbers that tell you whether exposure
# is working, from sources that already exist (no analytics account required).
#
#   task metrics          # or: bash scripts/metrics.sh
#
# Why this instead of a web-analytics snippet: the site has no traffic yet, so
# the measurable signal right now is upstream — release asset downloads and repo traffic. Those are the endpoints a channel actually
# moves, and they are readable today. Add site analytics later, when there is a
# site traffic number worth splitting by referrer.
set -uo pipefail

REPO="gqf2008/pi-rust"
hr
echo "GitHub release asset downloads (per platform)"
if command -v gh >/dev/null 2>&1; then
  gh api "repos/${REPO}/releases?per_page=5" \
    --jq '.[] | "  \(.tag_name)", (.assets[] | "    \(.download_count)\t\(.name)")' 2>/dev/null \
    || echo "  (gh api failed)"
fi

hr
echo "repo traffic, last 14 days (unique views / uniques, clones / uniques)"
if command -v gh >/dev/null 2>&1; then
  gh api "repos/${REPO}/traffic/views" \
    --jq '"  views \(.count) / \(.uniques)"' 2>/dev/null || echo "  views: (needs push access)"
  gh api "repos/${REPO}/traffic/clones" \
    --jq '"  clones \(.count) / \(.uniques)"' 2>/dev/null || echo "  clones: (needs push access)"
fi
hr

# Baseline for comparison. Fill a new row when you publish or submit something,
# so a channel's effect is attributable instead of remembered.
#
#   2026-09-26  site compare.html shipped           views __ / __   release __
#   2026-09-26  awesome-cli-coding-agents PR        views __ / __   release __
echo "Record a row per channel (see the comment at the end of this file)."
