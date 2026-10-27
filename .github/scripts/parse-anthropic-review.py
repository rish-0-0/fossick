#!/usr/bin/env python3
"""Extract {verdict, summary} JSON from Claude review message text."""
import json
import re
import sys


def fallback_obj(raw: str) -> dict | None:
    vm = re.search(r'"verdict"\s*:\s*"(PASS|FAIL)"', raw)
    if not vm:
        return None
    sm = re.search(r'"summary"\s*:\s*"', raw)
    if not sm:
        return {"verdict": vm.group(1), "summary": "summary missing from model output"}
    rest = raw[sm.end() :]
    end = len(rest)
    for pat in (r'"\s*}\s*$', r'"\s*,\s*"verdict"'):
        m = re.search(pat, rest)
        if m:
            end = m.start()
            break
    summary = rest[:end].replace("\\\\n", "\n").replace("\\n", "\n").replace('\\"', '"')
    return {"verdict": vm.group(1), "summary": summary}


def main() -> None:
    text = sys.stdin.read().strip()
    fenced = re.search(r"```(?:json)?\s*(\{.*\})\s*```", text, re.S)
    if fenced:
        text = fenced.group(1)
    else:
        start = text.find("{")
        if start < 0:
            sys.exit(1)
        text = text[start:]
    try:
        obj, _ = json.JSONDecoder().raw_decode(text)
    except json.JSONDecodeError:
        obj = fallback_obj(text)
    if not obj or not isinstance(obj.get("verdict"), str) or not isinstance(obj.get("summary"), str):
        sys.exit(1)
    print(json.dumps(obj))


if __name__ == "__main__":
    main()
