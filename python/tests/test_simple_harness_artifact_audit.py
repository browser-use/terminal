from __future__ import annotations

import csv
import json
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
AUDIT_SCRIPT = REPO_ROOT / "prompts" / "simple-harness-artifact-audit.py"


def run_audit(cwd: Path, task: str, *args: str) -> subprocess.CompletedProcess[str]:
    env = {"BROWSER_USE_TASK_TEXT": task}
    return subprocess.run(
        [sys.executable, str(AUDIT_SCRIPT), *args],
        cwd=cwd,
        text=True,
        capture_output=True,
        env=env,
        check=False,
    )


def test_artifact_audit_rejects_nested_missing_management_email(tmp_path: Path) -> None:
    result = tmp_path / "result.json"
    result.write_text(
        json.dumps(
            {
                "properties": [
                    {"name": "A", "contact": {"email": "", "phone": "817-111-1111"}},
                    {
                        "name": "B",
                        "contact": {
                            "email": "manager@example.com",
                            "phone": "817-222-2222",
                        },
                    },
                ]
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Collect property_info including management_email for two properties.",
    )

    assert audit.returncode == 2
    assert "nested `email` has 1/2 missing values" in audit.stdout


def test_artifact_audit_rejects_overwide_auction_end_time_filter(
    tmp_path: Path,
) -> None:
    result = tmp_path / "result.csv"
    with result.open("w", newline="") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=["Domain Name", "Auction End Time", "Sale Type"],
        )
        writer.writeheader()
        writer.writerow(
            {
                "Domain Name": "soon.example",
                "Auction End Time": "2026-06-18 12:00:00",
                "Sale Type": "Expiring Auction",
            }
        )
        writer.writerow(
            {
                "Domain Name": "later.example",
                "Auction End Time": "2026-06-22 12:00:00",
                "Sale Type": "Expiring Auction",
            }
        )

    audit = run_audit(
        tmp_path,
        "Under Hours to end, enter 24 and export Expiring Auctions.",
    )

    assert audit.returncode == 2
    assert "does not match the requested 24-hour end-time filter" in audit.stdout


def test_artifact_audit_accepts_tight_auction_end_time_filter(
    tmp_path: Path,
) -> None:
    result = tmp_path / "result.csv"
    with result.open("w", newline="") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=["Domain Name", "Auction End Time", "Sale Type"],
        )
        writer.writeheader()
        writer.writerow(
            {
                "Domain Name": "soon.example",
                "Auction End Time": "2026-06-18 12:00:00",
                "Sale Type": "Expiring Auction",
            }
        )
        writer.writerow(
            {
                "Domain Name": "later.example",
                "Auction End Time": "2026-06-19 11:30:00",
                "Sale Type": "Expiring Auction",
            }
        )

    audit = run_audit(
        tmp_path,
        "Under Hours to end, enter 24 and export Expiring Auctions.",
    )

    assert audit.returncode == 0
    assert "artifact-audit passed" in audit.stdout


def test_artifact_audit_rejects_blocked_article_text(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            [
                {
                    "headline": "Security Verification",
                    "url": "https://example.com/article",
                    "text": "Security Verification Status Code 403",
                }
            ]
        )
    )

    audit = run_audit(
        tmp_path,
        "Extract the full headline, URL, and complete article text for each article.",
    )

    assert audit.returncode == 2
    assert "blocker/security-verification text" in audit.stdout


def test_artifact_audit_rejects_visible_only_reviews_for_complete_list(
    tmp_path: Path,
) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "reviews_access_note": "Amazon redirected the all-reviews URL to a Sign-In page, so only visible reviews were extracted.",
                "visible_product_page_reviews": [{"author": "A", "body": "ok"}],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Open the highest rating palazzo and extract the reviews complete list.",
    )

    assert audit.returncode == 2
    assert "complete-list requirement" in audit.stdout


def test_artifact_audit_rejects_missing_required_ebay_coverage(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "browsed_sites": ["Amazon UK", "JIB", "eBay UK", "eBay USA"],
                "browser_blockers_or_caveats": [
                    "Exact part searches produced no listing items captured before timeouts."
                ],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Search amazon usa, and ebay uk and ebay usa, then match with JIB.",
    )

    assert audit.returncode == 2
    assert "required eBay marketplace coverage is missing" in audit.stdout


def test_artifact_audit_rejects_blocked_no_website_collection(tmp_path: Path) -> None:
    (tmp_path / "result.txt").write_text(
        "Unable to complete the Yelp data collection because Yelp blocked access. "
        "I could not reliably observe which businesses had no website."
    )

    audit = run_audit(
        tmp_path,
        "Find businesses listed WITHOUT a website. Return a list of businesses found with no website.",
        "result.txt",
    )

    assert audit.returncode == 2
    assert "no-website business collection is blocked or incomplete" in audit.stdout


def test_artifact_audit_rejects_listing_markers_with_missing_details(
    tmp_path: Path,
) -> None:
    (tmp_path / "result.txt").write_text(
        "LISTING START\n"
        "ID: 123\n"
        "Description: Not fully exposed in summary view\n"
        "Images: Raw URLs not exposed\n"
        "LISTING END\n"
    )

    audit = run_audit(
        tmp_path,
        "Format each listing with LISTING START and LISTING END markers. Include image URLs and description.",
        "result.txt",
    )

    assert audit.returncode == 2
    assert "listing extraction declares missing details" in audit.stdout


def test_artifact_audit_rejects_underfilled_platform_top_20(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "amazon_de": [{"platform": "amazon.de", "rank": 1}],
                "galaxus_de": [{"platform": "galaxus.de", "rank": 1}],
                "kaufland_de": [{"platform": "kaufland.de", "rank": 1}],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        'Find the top 20 selling "Nahrungsergänzungsmittel" on Amazon.de, Galaxus.de, and Kaufland.de.',
    )

    assert audit.returncode == 2
    assert "`amazon_de` has 1 rows, fewer than the requested top 20" in audit.stdout
    assert "`galaxus_de` has 1 rows, fewer than the requested top 20" in audit.stdout
    assert "`kaufland_de` has 1 rows, fewer than the requested top 20" in audit.stdout


def test_artifact_audit_rejects_non_supplement_products(tmp_path: Path) -> None:
    rows = [{"platform": "galaxus.de", "rank": idx + 1, "name": f"Vitamin D {idx}"} for idx in range(20)]
    rows[4]["name"] = "Gentle Face Cleanser"
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "amazon_de": [{"platform": "amazon.de", "rank": idx + 1} for idx in range(20)],
                "galaxus_de": rows,
                "kaufland_de": [{"platform": "kaufland.de", "rank": idx + 1} for idx in range(20)],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        'Find the top 20 selling "Nahrungsergänzungsmittel" on Amazon.de, Galaxus.de, and Kaufland.de.',
    )

    assert audit.returncode == 2
    assert "likely non-supplement products" in audit.stdout


def test_artifact_audit_rejects_dice_remote_and_old_rows(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "jobs": [
                    {
                        "Title": "Remote Developer",
                        "Description": "Remote role",
                        "Location": "Tallahassee, Florida, USA",
                        "PublicationDate": "5d ago",
                        "Source": "Dice.com",
                    }
                ]
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Navigate to Dice.com, enter Tallahassee, FL, apply Posted Date: Last 3 Days and Job Type: On-Site.",
    )

    assert audit.returncode == 2
    assert "mentions remote/hybrid despite on-site filter" in audit.stdout
    assert "PublicationDate `5d ago` is outside Last 3 Days" in audit.stdout


def test_artifact_audit_accepts_dice_last_three_day_onsite_rows(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "jobs": [
                    {
                        "Title": "Systems Administrator",
                        "Description": "100% On-site, No remote",
                        "Location": "Tallahassee, Florida, USA",
                        "PublicationDate": "2d ago",
                        "Source": "Dice.com",
                        "Deadline": None,
                    }
                ]
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Navigate to Dice.com, enter Tallahassee, FL, apply Posted Date: Last 3 Days and Job Type: On-Site. Deadline must be exactly null.",
    )

    assert audit.returncode == 0
    assert "artifact-audit passed" in audit.stdout
