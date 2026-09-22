"""Small offline regressions for maintained documentation validation."""

import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "doccheck", Path(__file__).resolve().parents[1] / "check_documentation.py"
)
doccheck = importlib.util.module_from_spec(spec)
spec.loader.exec_module(doccheck)


class DocumentationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.page = self.put("README.md", "# Test\n")

    def put(self, name, text):
        p = self.root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        return p

    def check(self, text):
        self.page.write_text(text)
        return doccheck.check(self.root, [self.page])

    def test_valid_local_link(self):
        self.put("docs/a.md", "# Title\n")
        self.assertEqual(self.check("# Root\n[A](docs/a.md)")["status"], "PASS")

    def test_missing_file(self):
        self.assertEqual(self.check("# Root\n[A](absent.md)")["status"], "FAIL")

    def test_valid_heading(self):
        self.put("docs/a.md", "# First title\n")
        self.assertEqual(
            self.check("# Root\n[A](docs/a.md#first-title)")["status"], "PASS"
        )

    def test_missing_heading(self):
        self.put("a.md", "# Title\n")
        self.assertEqual(self.check("# Root\n[A](a.md#nope)")["status"], "FAIL")

    def test_duplicate_heading(self):
        self.assertEqual(
            self.check("# Root\n## Same\n## Same\n[A](#same-1)")["status"], "PASS"
        )

    def test_unicode_heading(self):
        self.assertEqual(self.check("# Início\n[A](#in%C3%ADcio)")["status"], "PASS")

    def test_code_fences_not_links(self):
        self.assertEqual(
            self.check("# Root\n```md\n[X](absent.md)\n```\n")["status"], "PASS"
        )

    def test_inline_code_not_link(self):
        self.assertEqual(self.check("# Root\n`[X](absent.md)`")["status"], "PASS")

    def test_double_backtick_code_not_link(self):
        self.assertEqual(
            self.check("# Root\n``[X](absent`file.md)``")["status"], "PASS"
        )

    def test_code_label_remains_a_link(self):
        self.assertEqual(
            self.check("# Root\n[`Cargo.toml`](absent.md)")["status"], "FAIL"
        )

    def test_code_label_valid(self):
        self.put("a.md", "# Test\n")
        self.assertEqual(self.check("# Root\n[`code`](a.md)")["status"], "PASS")

    def test_inline_code_heading_anchor(self):
        self.assertEqual(self.check("# Use `Cargo`\n[X](#use-cargo)")["status"], "PASS")

    def test_code_title_is_not_page_title(self):
        self.assertEqual(self.check("```md\n# Example\n```")["status"], "FAIL")

    def test_tilde_fence(self):
        self.assertEqual(
            self.check("# Root\n~~~md\n[X](absent.md)\n~~~\n")["status"], "PASS"
        )

    def test_balanced_parentheses(self):
        self.put("test(1).md", "# Test\n")
        self.assertEqual(self.check("# Root\n[X](test(1).md)")["status"], "PASS")

    def test_angle_destination(self):
        self.put("with spaces.md", "# Test\n")
        self.assertEqual(self.check("# Root\n[X](<with spaces.md>)")["status"], "PASS")

    def test_percent_destination(self):
        self.put("with spaces.md", "# Test\n")
        self.assertEqual(self.check("# Root\n[X](with%20spaces.md)")["status"], "PASS")

    def test_root_relative(self):
        self.put("a.md", "# Test\n")
        self.assertEqual(self.check("# Root\n[X](/a.md)")["status"], "PASS")

    def test_parent_escape(self):
        self.assertEqual(self.check("# Root\n[X](../README.md)")["status"], "FAIL")

    def test_symlink_escape(self):
        (self.root / "outside").symlink_to("/etc/passwd")
        self.assertEqual(self.check("# Root\n[X](outside)")["status"], "FAIL")

    def test_reference_link(self):
        self.put("a.md", "# Test\n")
        self.assertEqual(self.check("# Root\n[X][ref]\n[ref]: a.md")["status"], "PASS")

    def test_missing_reference_link(self):
        self.assertEqual(self.check("# Root\n[X][ref]")["status"], "FAIL")

    def test_external_not_fetched(self):
        self.assertEqual(
            self.check("# Root\n[X](https://example.invalid/a#b)")["status"], "PASS"
        )

    def test_unsafe_scheme(self):
        self.assertEqual(
            self.check("# Root\n[X](javascript:alert(1))")["status"], "FAIL"
        )

    def test_image_requires_alt(self):
        self.put("a.png", "not read as an image")
        self.assertEqual(self.check("# Root\n![](a.png)")["status"], "FAIL")

    def test_missing_title(self):
        self.assertEqual(self.check("No title")["status"], "FAIL")

    def test_required_entries(self):
        self.assertEqual(doccheck.check(self.root)["status"], "FAIL")

    def test_oversized_instructions(self):
        p = self.put("AGENTS.md", "# Agents\n" + "line\n" * 201)
        self.assertEqual(doccheck.check(self.root, [p])["status"], "FAIL")

    def test_unexpanded_template(self):
        self.assertEqual(self.check("# Root\n{{PRODUCT}}")["status"], "FAIL")


if __name__ == "__main__":
    unittest.main()
