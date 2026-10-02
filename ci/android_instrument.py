import collections
import pathlib
import re
import sys
import xml.etree.ElementTree as element_tree


EXPECTED = {
    "native-load": {"NativeLoadTest": [
        "closureLoadsAndInitializesOnce",
        "rustPanicIsContainedAsRuntimeException",
    ]},
    "surface": {"SurfaceTest": ["rendersRetiresAndResumesOneLogicalWindow"]},
    "lifecycle": {
        "LifecycleTest": [
            "t1_rotationWhileTheEngineInitializesPaintsOnlyTheCurrentGeneration",
            "t2_backgroundingWithAFramePendingRetiresItAndResumesTheSameWindow",
            "t3_duplicateAndLateSurfaceEventsChangeNothing",
            "t4_clipboardReadRacingSurfaceDestructionCompletesWithoutACircularWait",
            "t5_backLeavesWindowsAliveAndReopenShowsTheSameWindow",
            "t6_selectorBindsAnotherWindowAndKeepsEveryWindow",
            "t7_idleEngineDoesNotRedrawAndLifecycleCyclesRetainNothing",
        ],
        "EngineFailureTest": [
            "guiThreadPanicWithAQueuedDestroyReleasesTheSurfaceAndTheUiThread",
            "guiThreadPanicRightAfterAClipboardReadStartedFailsThatRead",
            "bootstrapFailureBeforeAConnectionExistsReleasesTheRequestThread",
            "surfaceLostHandlerPanicStillReleasesTheNativeWindowInTheShutdown",
            "surfaceLostHandlerPanicInTheShutdownReportsTheSurfaceAsNotReleased",
        ],
    },
    "sshmux": {
        "SshMuxStartTest": ["connectWhileTheEngineStartsIsRefusedAndStartsNoAttempt"],
        "SshMuxTest": [
            "theKeyReadStopsAtItsLimitAndRefusesAZeroRead",
            "aNormalLaunchShowsTheConnectionScreenAndRefusesAddressesOutsideTheTailnet",
            "anUnknownHostIsRejectedThenTrustedOnceAndWrongPasswordsFailClosed",
            "aPendingPromptSurvivesLeavingTheActivityAndIsAnsweredOnce",
            "aChangedHostKeyFailsClosedAndKeepsTheTrustedKey",
            "anImportedIdentityAttachesToTheExistingPanesAndStartsNothing",
            "anEncryptedIdentityAsksForItsPassphraseThroughTheSecretPrompt",
            "anUnauthorizedIdentityFailsAuthenticationAndAttachesNothing",
            "aMissingMuxServerIsAVisibleFailureAndNothingIsStarted",
            "aCodecMismatchIsADistinctFailureAndAttachesNoPanes",
            "anEmptyMuxServerShowsTheEmptyStateAndSpawnsNothing",
        ],
    },
}


def entries(suite: str) -> list[str]:
    isolated = {"EngineFailureTest", "SshMuxStartTest", "SshMuxTest"}
    return [
        entry
        for class_name, methods in EXPECTED[suite].items()
        for entry in (
            [f"{class_name}#{method}" for method in methods]
            if class_name in isolated else [class_name]
        )
    ]


def parse(suite: str, entry: str, text: str, adb_exit: int) -> element_tree.Element:
    if adb_exit != 0:
        raise ValueError(f"adb exited {adb_exit}")
    if entry not in entries(suite):
        raise ValueError(f"unknown {suite} entry {entry}")
    class_name, separator, method = entry.partition("#")
    methods = [method] if separator else EXPECTED[suite][class_name]
    expected = collections.Counter((f"org.wezterm.android.{class_name}", name) for name in methods)
    started: collections.Counter[tuple[str, str]] = collections.Counter()
    completed: collections.Counter[tuple[str, str]] = collections.Counter()
    status: dict[str, str] = {}
    active: tuple[str, str] | None = None
    final_codes = []
    summaries = []
    result_started = False
    for line in text.splitlines():
        if line.startswith(("INSTRUMENTATION_FAILED:", "INSTRUMENTATION_ABORTED:", "FAILURES!!!")):
            raise ValueError(line)
        if final_codes and line.startswith("INSTRUMENTATION_"):
            raise ValueError("instrumentation continued after its final code")
        if line.startswith("INSTRUMENTATION_STATUS: "):
            key, equals, value = line.removeprefix("INSTRUMENTATION_STATUS: ").partition("=")
            if not equals or key in status:
                raise ValueError(f"malformed or duplicate status field: {line}")
            status[key] = value
        elif line.startswith("INSTRUMENTATION_STATUS_CODE: "):
            code = int(line.removeprefix("INSTRUMENTATION_STATUS_CODE: "))
            case = status.get("class", ""), status.get("test", "")
            if case not in expected or status.get("id") != "AndroidJUnitRunner":
                raise ValueError(f"unexpected status record: {status}")
            if int(status.get("numtests", "0")) != len(methods):
                raise ValueError("runner test count differs from the required inventory")
            if code == 1 and active is None and not started[case]:
                active = case
                started[case] += 1
            elif code == 0 and active == case and not completed[case]:
                active = None
                completed[case] += 1
            else:
                raise ValueError(f"failure/error/skip or unpaired status: {case} code={code}")
            if int(status.get("current", "0")) != sum(started.values()):
                raise ValueError("runner method sequence differs")
            status = {}
        elif line.startswith("INSTRUMENTATION_RESULT: "):
            result_started = True
        elif line.startswith("INSTRUMENTATION_CODE: "):
            final_codes.append(int(line.removeprefix("INSTRUMENTATION_CODE: ")))
        elif result_started and (summary := re.fullmatch(r"OK \((\d+) tests?\)", line)):
            summaries.append(int(summary[1]))
    if status or active or started != expected or completed != expected or final_codes != [-1] or summaries != [len(methods)]:
        raise ValueError(f"incomplete instrumentation: starts={started} completions={completed} expected={expected}")
    root = element_tree.Element("testsuite", name=f"org.wezterm.android.{class_name}", tests=str(len(methods)), failures="0", errors="0", skipped="0")
    for full_class, name in sorted(completed):
        element_tree.SubElement(root, "testcase", classname=full_class, name=name)
    return root


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "plan":
        print("\n".join(entries(sys.argv[2])))
    elif len(sys.argv) == 7 and sys.argv[1] == "parse":
        suite, entry, receipt, exit_receipt, output = sys.argv[2:]
        root = parse(suite, entry, pathlib.Path(receipt).read_text(), int(pathlib.Path(exit_receipt).read_text()))
        with pathlib.Path(output).open("xb") as stream:
            element_tree.ElementTree(root).write(stream, encoding="utf-8", xml_declaration=True)
        print(f"PASS {entry}: exact methods={root.attrib['tests']} failures=0 errors=0 skipped=0")
    else:
        sys.exit("usage: android_instrument.py plan <suite> | parse <suite> <entry> <raw-status> <adb-exit> <xml>")
