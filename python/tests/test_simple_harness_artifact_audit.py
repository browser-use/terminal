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


def test_artifact_audit_rejects_self_withheld_article_text(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "complete_article_text_fulfilled": False,
                "articles": [
                    {
                        "headline": "Article",
                        "url": "https://example.com/article",
                        "article_text": "N/A - copyrighted full text not reproduced",
                        "summary": "Short summary substituted for article body.",
                    }
                ],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Extract the full headline, URL, and complete article text for each article.",
    )

    assert audit.returncode == 2
    assert "complete article text is self-marked unavailable" in audit.stdout


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


def test_artifact_audit_accepts_task_allowed_na_fields(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            [
                {
                    "title": "TECHEU BAYER PHARMACEUTICAL RDI",
                    "reference_number": "20250676",
                    "submission_deadline": "N/A",
                    "estimated_budget": "EUR 1100 million",
                    "brief_description_of_scope": "Research and development pipeline project.",
                    "eligibility_criteria": "N/A",
                    "type_of_procedure": "N/A",
                    "dedicated_url": "https://www.eib.org/en/projects/pipelines/all/20250676",
                }
            ]
        )
    )

    audit = run_audit(
        tmp_path,
        "Extract title, reference number, submission deadline, estimated budget, brief description of the scope, eligibility criteria, type of procedure, and dedicated URL. If any piece of the requested information is unavailable, return 'N/A' for that field.",
    )

    assert audit.returncode == 0
    assert "artifact-audit passed" in audit.stdout


def test_artifact_audit_accepts_creator_websites_if_available(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "rows": [
                    {
                        "platform": "Kickstarter",
                        "game_name": f"Game {index}",
                        "project_url": f"https://example.com/project/{index}",
                        "description": "Upcoming tabletop game",
                        "date_added": "2026-06-19",
                        "creator_websites": "N/A",
                    }
                    for index in range(20)
                ]
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Create an automated report of upcoming tabletop game projects with clickable links to both the campaign and the creator's website. Each entry should show the creator's external website (if available).",
    )

    assert audit.returncode == 0
    assert "artifact-audit passed" in audit.stdout


def test_artifact_audit_rejects_creator_about_fetch_incomplete(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "sources": {
                    "kickstarter": {
                        "errors": [
                            {
                                "type": "profile_about_fetch_incomplete",
                                "message": "Creator About-page fetching was stopped because Kickstarter returned HTTP 429 throttling.",
                                "creator_profiles_not_fetched": 2443,
                            }
                        ]
                    }
                },
                "rows": [
                    {
                        "platform": "Kickstarter",
                        "game_name": "Game",
                        "project_url": "https://example.com/project",
                        "description": "Upcoming tabletop game",
                        "date_added": "2026-06-19",
                        "creator_about_urls": ["https://example.com/profile/about"],
                        "creator_websites": ["N/A"],
                    }
                ],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Create an automated report of upcoming tabletop game projects. For each project, visit the creator's About page and extract website link(s); creator website is if available.",
    )

    assert audit.returncode == 2
    assert "creator About-page website extraction skipped 2443 creator profiles" in audit.stdout


def test_artifact_audit_rejects_bounded_upcoming_marketplace_sample(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "metadata": {
                    "kickstarter_pages_fetched": 2,
                    "kickstarter_total_pages_available": 220,
                    "kickstarter_total_hits_reported": 2640,
                    "gamefound_pages_fetched": 2,
                    "gamefound_total_pages_available": 21,
                    "gamefound_total_hits_reported": 491,
                    "row_count": 72,
                    "scope_warning": "Default run is bounded to two newest pages per platform. Re-run with --all to attempt all pages.",
                },
                "rows": [
                    {
                        "Platform": "Kickstarter",
                        "Game Name": f"Game {index}",
                        "Game URL": f"https://example.com/project/{index}",
                        "Description": "Upcoming tabletop game",
                        "Date Added": "2026-06-19",
                        "Creator Websites": ["N/A"],
                    }
                    for index in range(72)
                ],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Create an automated report that compiles upcoming tabletop game projects from Kickstarter and Gamefound into a spreadsheet. Implement pagination handling.",
    )

    assert audit.returncode == 2
    assert "upcoming marketplace report is explicitly bounded" in audit.stdout
    assert "kickstarter pagination incomplete" in audit.stdout


def test_artifact_audit_rejects_eib_pipeline_no_tenders_answer(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            [
                {
                    "title": "No tender records found on the specified EIB projects pipeline page",
                    "reference_number": "N/A",
                    "brief_description_of_scope": "The specified URL lists EIB projects to be financed, not tender/procurement opportunities.",
                    "dedicated_url": "https://www.eib.org/en/projects/pipelines/index.htm",
                }
            ]
        )
    )

    audit = run_audit(
        tmp_path,
        "scrape tender information from https://www.eib.org/en/projects/pipelines/index.htm and extract title, reference number, submission deadline, estimated budget, brief description of the scope, eligibility criteria, type of procedure, and dedicated URL. If any piece of the requested information is unavailable, return 'N/A' for that field. Return the results as a JSON array of objects.",
    )

    assert audit.returncode == 2
    assert "EIB pipeline task was answered as no tenders" in audit.stdout


def test_artifact_audit_rejects_complete_false(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "complete": False,
                "records": [{"name": "Only partial page"}],
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Create a complete Excel report with pagination support.",
    )

    assert audit.returncode == 2
    assert "`complete` is false" in audit.stdout


def test_artifact_audit_rejects_ungm_non_it_scope_drift(tmp_path: Path) -> None:
    (tmp_path / "result.json").write_text(
        json.dumps(
            {
                "tenders": [
                    {
                        "rank": 1,
                        "title": "Extended Systematic Review and Meta-analysis on VAC",
                        "url": "https://www.ungm.org/Public/Notice/1",
                    },
                    {
                        "rank": 2,
                        "title": "Development of Policy Brief Zambia",
                        "url": "https://www.ungm.org/Public/Notice/1",
                    },
                    {
                        "rank": 3,
                        "title": "REOI - Cybersecurity Awareness Services",
                        "url": "https://www.ungm.org/Public/Notice/2",
                    },
                ]
            }
        )
    )

    audit = run_audit(
        tmp_path,
        "Scrape the first 20 tenders for UN-related IT projects from the UN Global Marketplace at https://www.ungm.org/Public/Notice",
    )

    assert audit.returncode == 2
    assert "UNGM IT-project result contains likely non-IT scope drift" in audit.stdout


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
