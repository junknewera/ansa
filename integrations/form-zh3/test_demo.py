import unittest

from demo import Demo


class DemoTest(unittest.TestCase):
    def test_replay_delivers_queued_text_only_at_the_boundary(self):
        demo = Demo()
        start = demo.state()
        self.assertEqual(start["clarifications"], [("Keep CSV format", "Recorded")])
        self.assertEqual(dict(start["checks"])["pytest -q"], "pending")
        self.assertFalse(start["pr_eligible"])
        self.assertFalse(demo.clarify("   "))
        self.assertTrue(demo.clarify("Keep the header"))
        self.assertEqual(demo.state()["clarifications"][-1], ("Keep the header", "Recorded"))
        tests = demo.advance()
        self.assertEqual(tests["phase"], "tests")
        self.assertEqual(tests["clarifications"], [
            ("Keep CSV format", "Delivered"), ("Keep the header", "Delivered"),
        ])
        self.assertFalse(demo.clarify("Late note"))
        review = demo.advance()
        self.assertEqual(review["phase"], "final_review")
        self.assertFalse(review["pr_eligible"])
        accepted = demo.advance()
        self.assertTrue(accepted["pr_eligible"])
        self.assertEqual(demo.advance(), accepted)
        self.assertEqual(demo.restart(), start)

    def test_selected_receipts_match_the_displayed_files(self):
        demo = Demo()
        offset, tests = demo.state()["documents"]
        self.assertEqual(offset["name"], "src/week.py")
        self.assertEqual(tests["name"], "tests/test_week.py")
        self.assertIn("-    offset = day.isoweekday() % 7 - 1", offset["diff"])
        self.assertIn("+    offset = day.weekday()", offset["diff"])
        self.assertIn("@@ -42,4 +42,4 @@", offset["diff"])
        self.assertIn("date(2026, 11, 16)", tests["source"])
        offset["source"] = "overwritten"
        self.assertNotEqual(demo.state()["documents"][0]["source"], offset["source"])


if __name__ == "__main__":
    unittest.main()
