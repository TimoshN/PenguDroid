import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location("compat_index", Path(__file__).with_name("compat-index.py"))
index = importlib.util.module_from_spec(spec)
spec.loader.exec_module(index)

class CompatibilityIndexTests(unittest.TestCase):
    def snapshot(self):
        return {"version":"test", "jni_function_slots":229, "wired_jni_slots":[4,6,6],
                "forbidden_android_runtime_mappings":[]}
    def catalog(self):
        return {"scope":"A fixed audit, not whole Android", "scenarios":[
            {"name":"smoke", "status":"passed", "evidence":"fixture"},
            {"name":"threads", "status":"missing", "evidence":"not implemented"}]}
    def test_counts_are_scoped_and_duplicate_slots_do_not_inflate_coverage(self):
        data = index.inventory(self.snapshot(), self.catalog(), [])
        self.assertEqual(data["jni"]["wired_slots"],[4,6])
        self.assertEqual(len(data["jni"]["placeholder_slots"]),227)
        self.assertIsNone(data["whole_android_compatibility_percent"])
        self.assertEqual(data["scenario_counts"],{"missing":1,"passed":1})
        root = ET.fromstring(index.svg(data))
        self.assertEqual(root.tag,"{http://www.w3.org/2000/svg}svg")
        self.assertIn("not Android conformance", index.svg(data))
    def test_refuses_forbidden_runtime_mappings_and_wrong_table_shape(self):
        snapshot = self.snapshot();snapshot["forbidden_android_runtime_mappings"]=["/lib/libart.so"]
        with self.assertRaises(ValueError):index.inventory(snapshot,self.catalog(),[])
        snapshot = self.snapshot();snapshot["wired_jni_slots"]=[3,233]
        with self.assertRaises(ValueError):index.inventory(snapshot,self.catalog(),[])
    def test_trace_observations_are_deduplicated_without_claiming_exhaustiveness(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"run.log"
            path.write_text("[pengu-vm] UNIMPLEMENTED method LTest;.missing()V\n" * 2 +
                "[pengu-jni] UNIMPLEMENTED JNI slot 164 called\n")
            data = index.inventory(self.snapshot(),self.catalog(),[path])
            self.assertEqual(data["observations"]["unimplemented_methods"],["LTest;.missing()V"])
            self.assertEqual(data["observations"]["unimplemented_jni_slots"],[164])
            self.assertFalse(data["observations"]["exhaustive"])
    def test_benchmark_uses_median_and_keeps_raw_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"bench.log"
            path.write_text("ns_per_call=10.0\nns_per_call=100.0\nns_per_call=12.0\n")
            result=index.benchmark(path)
            self.assertEqual(result["median_ns_per_call"],12.0)
            self.assertEqual(result["samples"],[10.0,100.0,12.0])

if __name__ == "__main__": unittest.main()
