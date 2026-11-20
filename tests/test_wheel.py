"""Exercise installed wheels on runtimes with limited test dependencies."""

import io
import unittest

import ryaml


class TestWheel(unittest.TestCase):
    def test_round_trip(self):
        value = {"object": [True, None, 42, 1.25, "café", b"binary"]}
        self.assertEqual(ryaml.loads(ryaml.dumps(value)), value)

        stream = io.StringIO()
        ryaml.dump(stream, value)
        stream.seek(0)
        self.assertEqual(ryaml.load(stream), value)

    def test_load_documents(self):
        self.assertEqual(ryaml.loads_all("--- 1\n--- [2, 3]\n"), [1, [2, 3]])

    def test_invalid_yaml(self):
        with self.assertRaises(ryaml.InvalidYamlError):
            ryaml.loads("key: [")

    def test_dumper_nesting_limit(self):
        value = "leaf"
        for _ in range(500):
            value = {"child": value}
        self.assertTrue(ryaml.dumps(value).endswith("child: leaf\n"))
        with self.assertRaises(RecursionError):
            ryaml.dumps({"child": value})
