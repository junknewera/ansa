"""In-memory weeklog fixture for Form Ж-3. No model, shell, Git or provider IO."""

from difflib import unified_diff


_TASK = "Fix Sunday week grouping and add a regression test"
_PLAN = (
    "Read week specification and confirm grouping rules",
    "Implement correct week offset and update tests",
    "Run tests and prepare final code for review",
)
_PHASES = ("local_review", "tests", "final_review", "accepted")

# This is the file excerpt selected in the design; its first line is line 42.
_OFFSET_BEFORE = """def _week_offset(day):
    # Compute offset from week start (Monday).
    offset = day.isoweekday() % 7 - 1
    return offset
"""
_OFFSET_AFTER = _OFFSET_BEFORE.replace("day.isoweekday() % 7 - 1", "day.weekday()")
_TEST_BEFORE = """from datetime import date, timedelta

from src.week import _week_offset


def test_monday_starts_the_week():
    assert _week_offset(date(2026, 11, 16)) == 0
"""
_TEST_AFTER = _TEST_BEFORE + '''

def test_sunday_belongs_to_the_week_starting_on_monday():
    sunday = date(2026, 11, 22)
    assert sunday - timedelta(days=_week_offset(sunday)) == date(2026, 11, 16)
'''


def _document(name, before, after, evidence, first_line=1):
    patch = list(unified_diff(
        before.splitlines(), after.splitlines(),
        fromfile=f"before edit_file/{name}", tofile=f"after edit_file/{name}",
        lineterm="",
    ))
    if first_line != 1:
        # The four-line excerpt is the whole hunk.
        patch[2] = f"@@ -{first_line},{len(before.splitlines())} +{first_line},{len(after.splitlines())} @@"
    return {
        "name": name, "before": before, "source": after,
        "diff": "\n".join(patch), "evidence": evidence, "first_line": first_line,
    }


_DOCUMENTS = (
    _document(
        "src/week.py", _OFFSET_BEFORE, _OFFSET_AFTER,
        "Demo receipt #16 · Weak · edit_file · isolated copy.\n"
        "spec/week_spec.md §2 · Weeks start on Monday. Sunday closes that week.\n"
        "2026-11-22: the old expression gives −1; weekday() gives 6.",
        first_line=42,
    ),
    _document(
        "tests/test_week.py", _TEST_BEFORE, _TEST_AFTER,
        "Demo receipt #14 · Weak · edit_file · isolated copy.\n"
        "The regression asserts that Sunday 22 November groups with Monday 16 November.\n"
        "The old offset would assign Sunday to Monday 23 November.",
    ),
)

_AGENT_STATUSES = {
    "local_review": ("patch returned", "reviewing step 2", "final review pending",
                     "reading docs · outside plan"),
    "tests": ("patch returned", "step 2 accepted", "final review pending",
              "week specification found · outside plan"),
    "final_review": ("patch returned", "step 2 accepted", "reviewing code and security",
                     "week specification found · outside plan"),
    "accepted": ("patch returned", "step 2 accepted", "final review accepted",
                 "week specification found · outside plan"),
}

_STEP_LABELS = {
    "local_review": "Step 2 · attempt 1/3",
    "tests": "Step 3 · candidate tests",
    "final_review": "Step 3 · final code and security review",
    "accepted": "Complete · candidate accepted",
}

_PYTEST_STATUS = {
    "local_review": "pending", "tests": "running",
    "final_review": "passed · 2 tests", "accepted": "passed · 2 tests",
}


class Demo:
    """Manual, deterministic replay. State is private to this instance."""

    def __init__(self):
        self._reset()

    def _reset(self):
        self._index = 0
        self._clarifications = [("Keep CSV format", "Recorded")]
        self._messages = [
            "#15 · 10:07 · /btw · reading spec/week_spec.md · outside plan",
            "#16 · 10:12 · Weak · edit_file · src/week.py · modified in isolated copy",
        ]

    def restart(self):
        self._reset()
        return self.state()

    def clarify(self, text):
        """Record a message in the open demo inbox. It is delivered on advance."""
        text = text.strip()
        if self._index != 0 or not text:
            return False
        self._clarifications.append((text, "Recorded"))
        return True

    def advance(self):
        """Advance one demo phase with no external effects; the last phase is stable."""
        if self._index == len(_PHASES) - 1:
            return self.state()
        self._index += 1
        phase = _PHASES[self._index]
        if phase == "tests":
            self._clarifications = [(text, "Delivered") for text, _ in self._clarifications]
            self._messages.extend([
                f"Demo coordinator · received {len(self._clarifications)} clarification(s) at the step boundary",
                "Demo Medium · accepted step 2 · clarification inbox closed",
                "Demo /btw · found the Monday rule in spec/week_spec.md · side conversation only",
                "Demo checks · pytest -q started on the isolated candidate",
            ])
        elif phase == "final_review":
            self._messages.extend([
                "Demo pytest -q · 2 passed · Monday and Sunday regression cases",
                "Demo Strong · final code and security review started",
            ])
        else:
            self._messages.append("Demo Strong · accepted the reviewed candidate · GitHub draft available for preview")
        return self.state()

    def state(self):
        phase = _PHASES[self._index]
        accepted = phase == "accepted"
        release = "No accepted candidate"
        if accepted:
            release = "Accepted candidate · demo draft preview available"
        return {
            "demo": True, "title": _TASK, "mode": "Orchestrate+", "project": "weeklog",
            "phase": phase, "step": _STEP_LABELS[phase], "plan": list(_PLAN),
            "agents": list(zip(("Weak / step 2", "Medium", "Strong", "/btw"), _AGENT_STATUSES[phase])),
            "sessions": ["weeklog", "/btw · read only", "fork", "History"],
            "messages": list(self._messages),
            "checks": [("pytest -q", _PYTEST_STATUS[phase])],
            "clarifications": list(self._clarifications),
            "documents": [document.copy() for document in _DOCUMENTS],
            "release": release, "pr_eligible": accepted,
            "can_advance": not accepted, "can_clarify": phase == "local_review",
        }
