import pathlib
import re
import unittest

import android_instrument as instrument


def receipt(class_name: str, methods: list[str]) -> str:
    records = []
    for index, method in enumerate(methods, 1):
        for code in (1, 0):
            records.append(
                f"INSTRUMENTATION_STATUS: class=org.wezterm.android.{class_name}\n"
                f"INSTRUMENTATION_STATUS: test={method}\n"
                "INSTRUMENTATION_STATUS: id=AndroidJUnitRunner\n"
                f"INSTRUMENTATION_STATUS: numtests={len(methods)}\n"
                f"INSTRUMENTATION_STATUS: current={index}\n"
                f"INSTRUMENTATION_STATUS_CODE: {code}\n"
            )
    return "".join(records) + f"INSTRUMENTATION_RESULT: stream=\n\nOK ({len(methods)} tests)\nINSTRUMENTATION_CODE: -1\n"


class InstrumentTest(unittest.TestCase):
    def setUp(self) -> None:
        self.methods = instrument.EXPECTED["native-load"]["NativeLoadTest"]
        self.text = receipt("NativeLoadTest", self.methods)

    def reject(self, text: str, adb_exit: int = 0, entry: str = "NativeLoadTest") -> None:
        with self.assertRaises(ValueError):
            instrument.parse("native-load", entry, text, adb_exit)

    def test_literal_pass(self) -> None:
        root = instrument.parse("native-load", "NativeLoadTest", self.text, 0)
        self.assertEqual(root.attrib, {"name": "org.wezterm.android.NativeLoadTest", "tests": "2", "failures": "0", "errors": "0", "skipped": "0"})
        self.assertEqual({case.attrib["name"] for case in root}, set(self.methods))

    def test_all_plans_keep_process_boundaries(self) -> None:
        self.assertEqual([len(instrument.entries(suite)) for suite in instrument.EXPECTED], [1, 1, 6, 14, 1, 13])
        for suite in instrument.EXPECTED:
            for entry in instrument.entries(suite):
                class_name, separator, method = entry.partition("#")
                methods = [method] if separator else instrument.EXPECTED[suite][class_name]
                root = instrument.parse(suite, entry, receipt(class_name, methods), 0)
                self.assertEqual(len(root), len(methods))

    def test_inventory_matches_unchanged_test_sources(self) -> None:
        directory = pathlib.Path(__file__).parents[1] / "android/app/src/androidTest/java/org/wezterm/android"
        for classes in instrument.EXPECTED.values():
            for class_name, methods in classes.items():
                source = (directory / f"{class_name}.kt").read_text()
                actual = re.findall(r"@Test\s+fun (\w+)\(", source)
                self.assertCountEqual(methods, actual, class_name)

    def test_failure(self) -> None:
        self.reject(self.text.replace("INSTRUMENTATION_STATUS_CODE: 0", "INSTRUMENTATION_STATUS_CODE: -2", 1))

    def test_error(self) -> None:
        self.reject(self.text.replace("INSTRUMENTATION_STATUS_CODE: 0", "INSTRUMENTATION_STATUS_CODE: -1", 1))

    def test_skipped(self) -> None:
        for code in (-3, -4):
            with self.subTest(code=code):
                self.reject(self.text.replace("INSTRUMENTATION_STATUS_CODE: 0", f"INSTRUMENTATION_STATUS_CODE: {code}", 1))

    def test_missing_method_with_ok_marker(self) -> None:
        self.reject(receipt("NativeLoadTest", self.methods[:1]))

    def test_incomplete(self) -> None:
        self.reject(self.text.split("INSTRUMENTATION_STATUS_CODE: 0", 1)[0])
        self.reject(self.text.replace("INSTRUMENTATION_CODE: -1", ""))

    def test_wrong_method(self) -> None:
        self.reject(self.text.replace(self.methods[0], "notTheRequiredMethod"))
        self.reject(self.text, entry="NativeLoadTest#notTheRequiredMethod")

    def test_input_suite_is_one_process_needing_every_method_in_order(self) -> None:
        self.assertEqual(instrument.entries("input"), ["InputTest"])
        methods = instrument.EXPECTED["input"]["InputTest"]
        self.assertEqual(methods, sorted(methods), "the methods share one connection in name order")
        root = instrument.parse("input", "InputTest", receipt("InputTest", methods), 0)
        self.assertEqual(len(root), len(methods), "one test case per inventory method")
        with self.assertRaises(ValueError):
            instrument.parse("input", "InputTest", receipt("InputTest", methods[:-1]), 0)
        with self.assertRaises(ValueError):
            instrument.parse("input", "InputTest#" + methods[0], receipt("InputTest", methods[:1]), 0)

    def test_reconnect_suite_runs_each_method_alone_and_the_force_stop_pair_in_order(self) -> None:
        methods = instrument.EXPECTED["reconnect"]["ReconnectTest"]
        entries = instrument.entries("reconnect")
        self.assertEqual(entries, [f"ReconnectTest#{method}" for method in methods])
        first = entries.index("ReconnectTest#forceStopWhileAttached1AttachesAndRecordsTheLaptopPanes")
        self.assertEqual(entries[first + 1], "ReconnectTest#forceStopWhileAttached2RelaunchAttachesTheSamePanesAgain")
        with self.assertRaises(ValueError):
            instrument.parse("reconnect", "ReconnectTest", receipt("ReconnectTest", methods), 0)
        with self.assertRaises(ValueError):
            instrument.parse("reconnect", entries[0], receipt("ReconnectTest", methods[1:2]), 0)
        with self.assertRaises(ValueError):
            instrument.parse("reconnect", entries[0], receipt("ReconnectTest", methods[:2]), 0)

    def test_adb_nonzero_with_valid_status(self) -> None:
        self.reject(self.text, adb_exit=255)

    def test_duplicate_or_unpaired_records(self) -> None:
        self.reject(self.text.replace("INSTRUMENTATION_STATUS_CODE: 1", "INSTRUMENTATION_STATUS_CODE: 0", 1))
        self.reject(self.text + self.text)

    def test_forged_summary_and_bad_final_code(self) -> None:
        self.reject("OK (2 tests)\nINSTRUMENTATION_CODE: -1\n")
        self.reject(self.text.replace("OK (2 tests)", "OK (20 tests)"))
        self.reject(self.text.replace("INSTRUMENTATION_CODE: -1", "INSTRUMENTATION_CODE: 0"))
        self.reject(self.text + "INSTRUMENTATION_FAILED: process crashed\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
