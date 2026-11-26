#!/usr/bin/env python3
"""Summarize pod-creation-to-Ready samples.

Each sample is a pair of files in one directory:

    <tag>.pod.json      kubectl get pod -o json
    <tag>.events.json   kubectl get events --field-selector involvedObject.name=... -o json

Stages, from pod condition and container timestamps plus pull events:

    image pull     Pulling/Pulled events (message duration when present)
    migrate        init container migrate startedAt -> finishedAt
    process        main container startedAt -> Ready condition
    total          pod creationTimestamp -> Ready condition

A pull is cached when the event says the image was already present, or the
reported duration is under one second. Prints every sample and the median
and max of each stage, split by cached vs not.
"""
import json
import statistics
import sys
from datetime import datetime
from pathlib import Path


def parse_ts(value):
    if not value:
        return None
    text = value.replace("Z", "+00:00")
    return datetime.fromisoformat(text)


def event_time(event):
    for key in ("eventTime", "firstTimestamp", "lastTimestamp"):
        stamp = parse_ts(event.get(key))
        if stamp:
            return stamp
    return None


def pull_duration(message):
    """Seconds from a kubelet pull message, or None."""
    import re
    match = re.search(r"\bin ([0-9.]+)\s*(µs|us|ms|s)\b", message or "")
    if not match:
        return None
    value = float(match.group(1))
    unit = match.group(2)
    if unit == "ms":
        return value / 1000
    if unit in ("us", "µs"):
        return value / 1_000_000
    return value


def record_pull(pulls, message, duration=None):
    cached_words = "already present" in message.lower()
    if duration is None:
        duration = pull_duration(message)
    if duration is None and cached_words:
        duration = 0.0
    cached = cached_words or (duration is not None and duration < 1.0)
    pulls.append({"message": message.strip(), "seconds": duration, "cached": cached})


def pull_events(events_doc):
    items = events_doc.get("items", []) if isinstance(events_doc, dict) else []
    items = sorted(items, key=lambda event: event_time(event) or parse_ts("1970-01-01T00:00:00Z"))
    pulls = []
    pending = None
    for event in items:
        reason = event.get("reason") or ""
        message = event.get("message") or ""
        if reason == "Pulling":
            pending = event
            continue
        cached_words = "already present" in message.lower()
        if reason in ("Pulled", "AlreadyPresent") or cached_words:
            duration = pull_duration(message)
            if duration is None and pending is not None:
                start = event_time(pending)
                end = event_time(event)
                if start and end:
                    duration = (end - start).total_seconds()
            record_pull(pulls, message, duration)
            pending = None
    return pulls


def pulls_from_describe(text):
    pulls = []
    pending = None
    for raw in text.splitlines():
        line = raw.strip()
        if "Pulling image" in line:
            pending = line
            continue
        if "Successfully pulled" in line or "already present" in line.lower():
            duration = pull_duration(line)
            record_pull(pulls, line, duration)
            pending = None
    return pulls


def condition_time(pod, name):
    for condition in pod.get("status", {}).get("conditions", []):
        if condition.get("type") == name and condition.get("status") == "True":
            return parse_ts(condition.get("lastTransitionTime"))
    return None


def container_times(pod, kind, name):
    statuses = pod.get("status", {}).get(kind, []) or []
    for status in statuses:
        if status.get("name") != name:
            continue
        state = status.get("state") or {}
        running = state.get("running") or {}
        terminated = state.get("terminated") or {}
        return parse_ts(running.get("startedAt") or terminated.get("startedAt")), parse_ts(terminated.get("finishedAt"))
    return None, None


def seconds(start, end):
    if start is None or end is None:
        return None
    return round((end - start).total_seconds(), 3)


def sample(tag, pod, events, describe=""):
    created = parse_ts(pod.get("metadata", {}).get("creationTimestamp"))
    ready = condition_time(pod, "Ready")
    migrate_start, migrate_end = container_times(pod, "initContainerStatuses", "migrate")
    proc_start, _ = container_times(pod, "containerStatuses", "modelrouter")
    pulls = pull_events(events)
    if not pulls and describe:
        pulls = pulls_from_describe(describe)
    pull_s = None
    if pulls and all(item["seconds"] is not None for item in pulls):
        pull_s = round(sum(item["seconds"] for item in pulls), 3)
    cached = None
    if pulls:
        cached = all(item["cached"] for item in pulls)
    return {
        "tag": tag,
        "cached": cached,
        "image_pull_s": pull_s,
        "migrate_s": seconds(migrate_start, migrate_end),
        "process_to_ready_s": seconds(proc_start, ready),
        "total_s": seconds(created, ready),
        "pulls": [item["message"] for item in pulls],
    }


def median(values):
    values = [value for value in values if value is not None]
    if not values:
        return None
    return round(statistics.median(values), 3)


def maximum(values):
    values = [value for value in values if value is not None]
    if not values:
        return None
    return round(max(values), 3)


def line(stats, stage):
    return f"  {stage:22} median {median(stats):7}  max {maximum(stats):7}  n {sum(v is not None for v in stats)}"


def report(directory):
    root = Path(directory)
    rows = []
    for pod_path in sorted(root.glob("*.pod.json")):
        tag = pod_path.name[: -len(".pod.json")]
        events_path = root / f"{tag}.events.json"
        events = json.loads(events_path.read_text()) if events_path.exists() else {"items": []}
        describe_path = root / f"{tag}.describe.txt"
        describe = describe_path.read_text(errors="replace") if describe_path.exists() else ""
        pod = json.loads(pod_path.read_text())
        rows.append(sample(tag, pod, events, describe))
    print(f"samples {len(rows)}")
    for row in rows:
        cache = {True: "cached", False: "not-cached", None: "unknown"}[row["cached"]]
        print(
            f"{row['tag']:40} {cache:10} pull {row['image_pull_s']}  "
            f"migrate {row['migrate_s']}  process {row['process_to_ready_s']}  total {row['total_s']}"
        )
        for message in row["pulls"]:
            print(f"    pull: {message}")
    stages = ("image_pull_s", "migrate_s", "process_to_ready_s", "total_s")
    print("ALL")
    for stage in stages:
        print(line([row[stage] for row in rows], stage))
    for label, cached in (("CACHED", True), ("NOT_CACHED", False)):
        group = [row for row in rows if row["cached"] is cached]
        print(f"{label} n={len(group)}")
        for stage in stages:
            print(line([row[stage] for row in group], stage))
    if len(rows) < 5:
        print(f"ERROR fewer than 5 samples ({len(rows)})")
        return 1
    return 0


def main():
    if len(sys.argv) != 2:
        print("usage: timing.py DIRECTORY", file=sys.stderr)
        return 2
    return report(sys.argv[1])


if __name__ == "__main__":
    sys.exit(main())
