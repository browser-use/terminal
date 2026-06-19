#!/usr/bin/env python3
import csv
import json
import os
import re
import sys
from datetime import datetime
from pathlib import Path

PLACEHOLDER = {
    "",
    "n/a",
    "na",
    "none",
    "null",
    "unknown",
    "missing",
    "not found",
    "unavailable",
    "not available",
    "not listed",
    "not checked",
    "not displayed",
    "not displayed on website",
    "not extracted",
    "not provided",
    "not shown",
    "not shown on website",
    "not supplied",
    "could not determine",
}

CORE_FIELD_HINTS = (
    "brand",
    "category",
    "connection",
    "contract",
    "email",
    "image",
    "market",
    "name",
    "package",
    "price",
    "rating",
    "review",
    "speed",
    "supplier",
    "url",
    "wallet",
)


def main() -> int:
    task = os.environ.get("BROWSER_USE_TASK_TEXT", "")
    paths = [Path(arg) for arg in sys.argv[1:] if not arg.startswith("-")]
    if not paths:
        paths = discover_default_paths()
    if not paths:
        print("artifact-audit: no result file found; pass result.json, result.csv, result.md, or result.txt")
        return 2

    issues = []
    inspected = []
    for path in paths:
        if not path.exists():
            issues.append(f"{path}: file does not exist")
            continue
        if path.stat().st_size == 0:
            issues.append(f"{path}: file is empty")
            continue
        inspected.append(str(path))
        try:
            doc = load_document(path)
        except Exception as exc:
            issues.append(f"{path}: could not parse ({exc})")
            continue
        issues.extend(audit_document(path, doc, task))

    if inspected:
        print("artifact-audit inspected: " + ", ".join(inspected))
    if issues:
        print("artifact-audit found blocking issues:")
        for issue in issues:
            print(f"- {issue}")
        return 2
    print("artifact-audit passed: no obvious empty, incomplete, or off-scope artifact shape detected")
    return 0


def discover_default_paths():
    preferred = []
    for name in ("result.json", "result.csv", "result.md", "result.txt"):
        path = Path(name)
        if path.exists():
            preferred.append(path)
    if preferred:
        return preferred[:1]
    return sorted(Path(".").glob("*.json"))[:1] or sorted(Path(".").glob("*.csv"))[:1]


def load_document(path):
    suffix = path.suffix.lower()
    if suffix == ".json":
        return json.loads(path.read_text(encoding="utf-8"))
    if suffix == ".csv":
        with path.open(newline="", encoding="utf-8") as handle:
            return list(csv.DictReader(handle))
    return path.read_text(encoding="utf-8", errors="replace")


def audit_document(path, doc, task):
    issues = []
    rows = []
    arrays = []
    text = ""

    if isinstance(doc, str):
        text = doc
    else:
        text = json.dumps(doc, ensure_ascii=False)
        collect_rows_and_arrays(doc, rows, arrays)
        if is_empty_result(doc, arrays):
            issues.append(f"{path}: result is empty or all result arrays are empty")
        issues.extend(audit_rows(path, rows, task))
        issues.extend(audit_expected_groups(path, rows, doc, task))
        issues.extend(audit_nested_required_fields(path, doc, task))
        issues.extend(audit_time_filters(path, rows, task))
        issues.extend(audit_dice_scope(path, rows, task))

    issues.extend(audit_task_specific_text(path, text, task))
    return dedupe(issues)


def collect_rows_and_arrays(value, rows, arrays):
    if isinstance(value, list):
        arrays.append(value)
        if all(isinstance(item, dict) for item in value):
            rows.extend(value)
        for item in value:
            collect_rows_and_arrays(item, rows, arrays)
    elif isinstance(value, dict):
        for child in value.values():
            collect_rows_and_arrays(child, rows, arrays)


def is_empty_result(doc, arrays):
    if doc == [] or doc == {}:
        return True
    if arrays and all(len(items) == 0 for items in arrays):
        return True
    if isinstance(doc, dict):
        list_values = [value for value in doc.values() if isinstance(value, list)]
        return bool(list_values) and all(len(value) == 0 for value in list_values)
    return False


def audit_rows(path, rows, task):
    if not rows:
        return []
    issues = []
    keys = sorted({str(key) for row in rows for key in row.keys()})
    lower_task = task.lower()
    allow_unavailable_fields = task_explicitly_allows_unavailable_fields(lower_task)
    for key in keys:
        key_lower = key.lower()
        if key_lower.replace("_", " ") in {"discounted price", "discounted_price"}:
            continue
        if key_lower == "deadline" and "deadline must be exactly null" in lower_task:
            continue
        values = [row.get(key) for row in rows if isinstance(row, dict) and key in row]
        if not values:
            continue
        missing = sum(is_missing(value) for value in values)
        ratio = missing / max(len(values), 1)
        if field_allows_unavailable_values(key_lower, lower_task):
            continue
        if allow_unavailable_fields and missing == len(values) and all(is_explicit_na(value) for value in values):
            continue
        if missing == len(values) and is_likely_required_field(key_lower, lower_task):
            issues.append(f"{path}: field `{key}` is missing/null/placeholder for every row")
        elif "email" in key_lower and missing > 0 and "if no direct email" in lower_task:
            issues.append(
                f"{path}: `{key}` has {missing}/{len(values)} missing values despite the task requiring a general-email fallback"
            )
        elif len(values) >= 10 and ratio >= 0.20 and "brand" in key_lower and "brand name" in lower_task:
            issues.append(f"{path}: `{key}` has {missing}/{len(values)} missing brand values")
        elif len(values) >= 10 and ratio >= 0.25 and "review" in key_lower and "review count" in lower_task:
            issues.append(f"{path}: `{key}` has {missing}/{len(values)} missing review counts")
        elif len(values) >= 10 and ratio >= 0.80 and is_likely_required_field(key_lower, lower_task):
            issues.append(f"{path}: `{key}` has {missing}/{len(values)} missing or placeholder values")

    if "40 total leads" in lower_task and len(rows) >= 40:
        for key in ("Business Name", "Category", "Review Count"):
            if key in keys:
                missing = sum(is_missing(row.get(key)) for row in rows)
                if missing:
                    issues.append(f"{path}: lead field `{key}` has {missing}/{len(rows)} missing values")
    return issues


def audit_expected_groups(path, rows, doc, task):
    lower_task = task.lower()
    lower_text = json.dumps(doc, ensure_ascii=False).lower()
    issues = []

    if "dsl/fiber" in lower_task or ("dsl" in lower_task and "fiber" in lower_task):
        if "dsl" in lower_text and "fiber" not in lower_text and "glasfaser" not in lower_text:
            issues.append(f"{path}: task asks for DSL/Fiber coverage but artifact appears to contain only DSL rows")

    if "kauppa.dna.fi/laajakaista" in lower_task and "all package" in lower_task:
        has_dna_5g = "dna" in lower_text and "5g" in lower_text
        has_fixed_marker = any(marker in lower_text for marker in ("fiber", "fibre", "valokuitu", "kiinte", "10 mbit", "150 mbit"))
        if has_dna_5g and not has_fixed_marker:
            issues.append(f"{path}: DNA laajakaista extraction appears to contain only 5G rows; verify fixed broadband/fiber tiers before finalizing")

    for expected in expected_provider_names(task):
        if expected.lower() not in lower_text:
            issues.append(f"{path}: required source/provider `{expected}` has no rows or evidence")

    issues.extend(audit_platform_array_counts(path, doc, lower_task))

    if "samlino broadband" in lower_task:
        if re.search(r'"task_?2[^"]*"\s*:\s*{[^{}]*"packages"\s*:\s*\[\s*\]', lower_text):
            issues.append(f"{path}: Samlino Broadband packages are empty")
        if "samlino" in lower_text and "broadband" in lower_text and re.search(r'"packages"\s*:\s*\[\s*\]', lower_text):
            issues.append(f"{path}: at least one required packages array is empty")

    issues.extend(audit_incomplete_markers(path, doc, lower_task))
    issues.extend(audit_ungm_it_scope(path, doc, lower_task))
    issues.extend(audit_eib_pipeline_scope(path, rows, doc, lower_task))
    issues.extend(audit_creator_website_fetch_completeness(path, doc, lower_task))

    return issues


def audit_platform_array_counts(path, doc, lower_task):
    if not isinstance(doc, dict):
        return []
    if "top 20" not in lower_task:
        return []

    expected = []
    if "amazon.de" in lower_task:
        expected.append(("amazon_de", "amazon.de"))
    if "galaxus.de" in lower_task:
        expected.append(("galaxus_de", "galaxus.de"))
    if "kaufland.de" in lower_task:
        expected.append(("kaufland_de", "kaufland.de"))
    if not expected:
        return []

    issues = []
    for key, platform in expected:
        value = doc.get(key)
        if not isinstance(value, list):
            issues.append(f"{path}: expected top-20 array `{key}` for {platform} is missing")
            continue
        if len(value) < 20:
            issues.append(f"{path}: `{key}` has {len(value)} rows, fewer than the requested top 20")
        mismatched = 0
        for row in value:
            if isinstance(row, dict):
                seen = str(row.get("platform", "")).strip().lower()
                if seen and seen != platform:
                    mismatched += 1
        if mismatched:
            issues.append(f"{path}: `{key}` has {mismatched}/{len(value)} rows with the wrong platform value")
    return issues


def audit_nested_required_fields(path, doc, task):
    lower_task = task.lower()
    issues = []

    if "management_email" in lower_task or "management email" in lower_task:
        emails = find_values_for_key(doc, "email")
        if not emails:
            issues.append(f"{path}: task requires management_email but no nested `email` fields were found")
        else:
            missing = sum(is_missing(value) for value in emails)
            if missing:
                issues.append(f"{path}: nested `email` has {missing}/{len(emails)} missing values despite required management_email")

    return issues


def audit_time_filters(path, rows, task):
    lower_task = task.lower()
    if not rows:
        return []
    if "hours to end" not in lower_task or "24" not in lower_task:
        return []

    time_key = None
    for key in rows[0].keys():
        if "auction end time" == str(key).strip().lower() or (
            "end" in str(key).lower() and "time" in str(key).lower()
        ):
            time_key = key
            break
    if time_key is None:
        return []

    parsed = []
    for row in rows:
        value = row.get(time_key)
        if is_missing(value):
            continue
        timestamp = parse_datetime_like(str(value))
        if timestamp is not None:
            parsed.append(timestamp)
    if len(parsed) < 2:
        return []

    span_hours = (max(parsed) - min(parsed)).total_seconds() / 3600.0
    if span_hours > 30:
        return [
            f"{path}: `{time_key}` spans {span_hours:.1f} hours, which does not match the requested 24-hour end-time filter"
        ]
    return []


def audit_dice_scope(path, rows, task):
    lower_task = task.lower()
    if not rows or "dice.com" not in lower_task:
        return []

    issues = []
    task_requires_tallahassee = "tallahassee" in lower_task
    task_requires_onsite = "on-site" in lower_task or "onsite" in lower_task
    task_requires_last_three_days = "last 3 days" in lower_task or "last three days" in lower_task

    for idx, row in enumerate(rows, start=1):
        if not isinstance(row, dict):
            continue
        joined = " ".join(str(value) for value in row.values() if value is not None).lower()
        source = str(row.get("Source", row.get("source", ""))).lower()
        if source and source != "dice.com":
            issues.append(f"{path}: Dice row {idx} has Source `{source}`, expected Dice.com")
        if task_requires_tallahassee:
            location = str(row.get("Location", row.get("location", ""))).lower()
            if "tallahassee" not in location and "tallahassee" not in joined:
                issues.append(f"{path}: Dice row {idx} does not show Tallahassee scope")
        if task_requires_onsite and re.search(r"\b(remote|hybrid)\b", joined):
            if "no remote" not in joined and "100% on-site" not in joined and "100% onsite" not in joined:
                issues.append(f"{path}: Dice row {idx} mentions remote/hybrid despite on-site filter")
        if task_requires_last_three_days:
            posted = str(
                row.get("PublicationDate", row.get("publication_date", row.get("posted", "")))
            ).lower()
            if posted and not is_last_three_days_label(posted):
                issues.append(f"{path}: Dice row {idx} PublicationDate `{posted}` is outside Last 3 Days")
    return issues


def audit_task_specific_text(path, text, task):
    lower_task = task.lower()
    lower_text = text.lower()
    issues = []

    if "operator id" in lower_task and "ctb no" in lower_text:
        issues.append(f"{path}: answer cites CTB No, which is not the requested operator ID")

    if "same ssds" in lower_task or "same ssd" in lower_task:
        for left, right in (("t710", "t700"), ("990 pro heatsink", "990 pro intern"), ("9100 pro med", "9100 pro intern")):
            if left in lower_text and right in lower_text:
                issues.append(f"{path}: SSD comparison appears to mix distinct product models/options ({left} vs {right})")

    if ("complete article text" in lower_task or "complete text of the article" in lower_task) and any(
        marker in lower_text
        for marker in ("security verification", "status code 403", "just a moment", "captcha")
    ):
        issues.append(f"{path}: article extraction includes blocker/security-verification text instead of complete article text")

    if "complete article text" in lower_task or "complete text of the article" in lower_task:
        if any(
            marker in lower_text
            for marker in (
                "complete_article_text_fulfilled\": false",
                "complete article text fulfilled\": false",
                "copyrighted full text not reproduced",
                "article_text\": \"n/a",
                "article text: n/a",
                "full text unavailable",
                "full text withheld",
            )
        ):
            issues.append(f"{path}: complete article text is self-marked unavailable or replaced with summaries")

    if "review" in lower_task and "complete list" in lower_task and any(
        marker in lower_text
        for marker in ("sign-in page", "redirected to sign-in", "only the complete visible", "only visible")
    ):
        issues.append(f"{path}: review extraction is limited to visible/auth-unblocked reviews despite complete-list requirement")

    if "ebay" in lower_task and any(
        marker in lower_text
        for marker in ("no listing items captured", "no parsed listing", "browser harness became unstable", "timeouts")
    ):
        issues.append(f"{path}: required eBay marketplace coverage is missing or explicitly source-limited")

    if ("without a website" in lower_task or "no website" in lower_task) and any(
        marker in lower_text
        for marker in (
            "unable to complete",
            "blocked access",
            "you have been blocked",
            "verification page",
            "could not reliably observe",
            "could not collect",
            "could not complete",
        )
    ):
        issues.append(f"{path}: no-website business collection is blocked or incomplete")

    if "listing start" in lower_task and any(
        marker in lower_text
        for marker in (
            "not fully exposed",
            "raw urls not exposed",
            "raw url not exposed",
            "not available / hidden",
            "blocked by",
            "security",
            "validation page",
        )
    ):
        issues.append(f"{path}: listing extraction declares missing details or source blocking")

    if "nahrungserg" in lower_task or "dietary supplement" in lower_task:
        non_products = ("book", "buch", "ratgeber", "kompass", "therapy", "skincare", "cleanser", "soap", "seife")
        hits = [word for word in non_products if word in lower_text]
        if hits:
            issues.append(f"{path}: supplement ranking contains likely non-supplement products ({', '.join(sorted(set(hits)))})")

    if "40 candidates per specialty" in lower_task:
        if "broad/inferred" in lower_text or "inferred candidate" in lower_text or "procedure list" in lower_text:
            issues.append(f"{path}: specialty coverage is marked inferred or unsupported, not evidence-backed per specialty")
        if re.search(r'"specialties"\s*:\s*\[\s*\]', lower_text):
            issues.append(f"{path}: specialty rows include empty specialties despite per-specialty candidate requirement")

    return issues


def audit_ungm_it_scope(path, doc, lower_task):
    if "ungm" not in lower_task and "un global marketplace" not in lower_task:
        return []
    if "it project" not in lower_task and "it-related" not in lower_task and "information technology" not in lower_task:
        return []

    rows = []
    if isinstance(doc, dict) and isinstance(doc.get("tenders"), list):
        rows = [row for row in doc["tenders"] if isinstance(row, dict)]
    elif isinstance(doc, list):
        rows = [row for row in doc if isinstance(row, dict)]
    if not rows:
        return []

    non_it_markers = (
        "meta-analysis",
        "meta analysis",
        "policy brief",
        "communication materials",
        "consultant for communication",
        "videography",
        "costing and financing",
        "electoral process",
    )
    positive_it_markers = (
        "api",
        "application",
        "cyber",
        "data centre",
        "data center",
        "database",
        "dhis2",
        "digital",
        "ict",
        "information system",
        "information technology",
        "lms",
        "network",
        "platform",
        "software",
        "system",
        "technology",
        "ui",
        "ux",
        "vapt",
        "web",
    )

    bad = []
    for index, row in enumerate(rows, start=1):
        title = str(row.get("title") or row.get("detail_title") or row.get("name") or "")
        description = str(row.get("description") or "")
        haystack = f"{title} {description}".lower()
        has_non_it = any(contains_phrase(haystack, marker) for marker in non_it_markers)
        has_positive_it = any(contains_phrase(haystack, marker) for marker in positive_it_markers)
        if has_non_it and not has_positive_it:
            bad.append((index, title[:120] or "(untitled)"))

    if not bad:
        return []
    examples = "; ".join(f"row {index}: {title}" for index, title in bad[:5])
    return [f"{path}: UNGM IT-project result contains likely non-IT scope drift ({examples})"]


def audit_eib_pipeline_scope(path, rows, doc, lower_task):
    if "eib.org/en/projects/pipelines" not in lower_task:
        return []
    if "tender" not in lower_task and "pipeline" not in lower_task:
        return []

    lower_text = json.dumps(doc, ensure_ascii=False).lower()
    if "no tender records found" in lower_text or "not a tender/procurement" in lower_text:
        return [f"{path}: EIB pipeline task was answered as no tenders instead of extracting pipeline records"]

    if rows and len(rows) < 100:
        return [f"{path}: EIB pipeline extraction has only {len(rows)} rows; expected many pipeline records"]
    return []


def audit_creator_website_fetch_completeness(path, doc, lower_task):
    if "creator" not in lower_task or "website" not in lower_task:
        return []
    if "about page" not in lower_task and "about-page" not in lower_task and "/about" not in lower_task:
        return []

    issues = []
    not_fetched_values = find_values_for_key(doc, "creator_profiles_not_fetched")
    for value in not_fetched_values:
        if isinstance(value, int) and value > 0:
            issues.append(f"{path}: creator About-page website extraction skipped {value} creator profiles")

    lower_text = json.dumps(doc, ensure_ascii=False).lower()
    if "profile_about_fetch_incomplete" in lower_text or "about-page fetching was stopped" in lower_text:
        issues.append(f"{path}: creator About-page website extraction is self-marked incomplete")
    return issues


def contains_phrase(text, phrase):
    escaped = re.escape(phrase)
    return bool(re.search(rf"(?<![a-z0-9]){escaped}(?![a-z0-9])", text))


def parse_datetime_like(value):
    text = value.strip()
    for fmt in (
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M",
    ):
        try:
            return datetime.strptime(text, fmt)
        except ValueError:
            pass
    return None


def is_last_three_days_label(value):
    text = value.strip().lower()
    if text in {"today", "yesterday", "just posted", "new"}:
        return True
    if re.search(r"\b([0-9]|1[0-9]|2[0-3])\s*(h|hr|hrs|hour|hours)\s*ago\b", text):
        return True
    match = re.search(r"\b([0-3])\s*(d|day|days)\s*ago\b", text)
    if match:
        return int(match.group(1)) <= 3
    return False


def audit_incomplete_markers(path, doc, lower_task):
    issues = []
    if not task_explicitly_allows_incomplete_artifact(lower_task):
        for key in ("complete", "is_complete", "ready_for_done"):
            for value in find_values_for_key(doc, key):
                if value is False:
                    issues.append(f"{path}: `{key}` is false")

    missing_requirements = find_values_for_key(doc, "missing_requirements")
    for value in missing_requirements:
        if isinstance(value, list) and value:
            issues.append(f"{path}: artifact declares non-empty missing_requirements")
        elif isinstance(value, str) and value.strip():
            issues.append(f"{path}: artifact declares missing_requirements")

    for key in ("status", "extraction_status"):
        for value in find_values_for_key(doc, key):
            if isinstance(value, str) and "incomplete" in value.lower():
                issues.append(f"{path}: `{key}` declares the result incomplete")
    return issues


def find_values_for_key(value, key):
    found = []
    if isinstance(value, dict):
        for candidate, child in value.items():
            if str(candidate).lower() == key:
                found.append(child)
            found.extend(find_values_for_key(child, key))
    elif isinstance(value, list):
        for child in value:
            found.extend(find_values_for_key(child, key))
    return found


def expected_provider_names(task):
    names = []
    for name in ("DNA", "Telia", "Samlino", "YouSee", "Norlys", "Amazon", "Galaxus", "Kaufland"):
        if name.lower() in task.lower():
            names.append(name)
    return names


def is_likely_required_field(key, lower_task):
    normalized = key.replace("_", " ")
    if normalized in lower_task:
        return True
    return any(hint in key and hint in lower_task for hint in CORE_FIELD_HINTS)


def task_explicitly_allows_unavailable_fields(lower_task):
    return bool(
        re.search(r"if .*unavailable.*return ['`\"]?n/?a", lower_task)
        or re.search(r"if .*unavailable.*use ['`\"]?n/?a", lower_task)
        or re.search(r"return ['`\"]?n/?a['`\"]? for (that|the) field", lower_task)
        or re.search(r"use ['`\"]?n/?a['`\"]? (for|when).*unavailable", lower_task)
    )


def field_allows_unavailable_values(key_lower, lower_task):
    normalized = key_lower.replace("_", " ")
    if "if available" not in lower_task:
        return False
    if "creator" in normalized and ("website" in normalized or "websites" in normalized):
        return "creator" in lower_task and "website" in lower_task
    return False


def task_explicitly_allows_incomplete_artifact(lower_task):
    return "partial result" in lower_task or "incomplete result" in lower_task


def is_explicit_na(value):
    if not isinstance(value, str):
        return False
    normalized = re.sub(r"\s+", " ", value.strip().lower())
    return normalized in {"n/a", "na"} or normalized.startswith("n/a ")


def is_missing(value):
    if value is None:
        return True
    if isinstance(value, str):
        normalized = re.sub(r"\s+", " ", value.strip().lower())
        return (
            normalized in PLACEHOLDER
            or normalized.startswith("n/a")
            or normalized.startswith("not found")
            or normalized.startswith("not displayed")
            or normalized.startswith("not provided")
            or normalized.startswith("not shown")
            or normalized.startswith("not supplied")
            or normalized.startswith("could not ")
            or normalized.startswith("unable to ")
        )
    if isinstance(value, list):
        return len(value) == 0
    return False


def dedupe(items):
    seen = set()
    out = []
    for item in items:
        if item not in seen:
            seen.add(item)
            out.append(item)
    return out


if __name__ == "__main__":
    raise SystemExit(main())
