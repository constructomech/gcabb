#!/usr/bin/env swift

import ApplicationServices
import Foundation

guard CommandLine.arguments.count == 2,
      let rawPID = Int32(CommandLine.arguments[1])
else {
    fputs("usage: validate_macos_accessibility.swift <gcabb-pid>\n", stderr)
    exit(2)
}

guard AXIsProcessTrusted() else {
    fputs(
        "accessibility access is disabled for this terminal or host application\n",
        stderr
    )
    exit(2)
}

func attribute(_ element: AXUIElement, _ name: CFString) -> CFTypeRef? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name, &value) == .success else {
        return nil
    }
    return value
}

func text(_ element: AXUIElement, _ name: CFString) -> String? {
    attribute(element, name) as? String
}

func boolean(_ element: AXUIElement, _ name: CFString) -> Bool {
    guard let value = attribute(element, name),
          CFGetTypeID(value) == CFBooleanGetTypeID()
    else {
        return false
    }
    return CFBooleanGetValue((value as! CFBoolean))
}

func actions(_ element: AXUIElement) -> [String] {
    var names: CFArray?
    guard AXUIElementCopyActionNames(element, &names) == .success else {
        return []
    }
    return names as? [String] ?? []
}

func children(_ element: AXUIElement) -> [AXUIElement] {
    attribute(element, kAXChildrenAttribute as CFString) as? [AXUIElement] ?? []
}

func find(_ identifier: String, in element: AXUIElement) -> AXUIElement? {
    if text(element, kAXIdentifierAttribute as CFString) == identifier {
        return element
    }
    for child in children(element) {
        if let match = find(identifier, in: child) {
            return match
        }
    }
    return nil
}

func waitForElement(_ identifier: String, in app: AXUIElement) -> AXUIElement? {
    for _ in 0..<50 {
        if let element = find(identifier, in: app) {
            return element
        }
        Thread.sleep(forTimeInterval: 0.1)
    }
    return nil
}

func fail(_ message: String) -> Never {
    fputs("\(message)\n", stderr)
    exit(1)
}

let app = AXUIElementCreateApplication(pid_t(rawPID))
var appRole: CFTypeRef?
guard AXUIElementCopyAttributeValue(
    app,
    kAXRoleAttribute as CFString,
    &appRole
) == .success
else {
    fail("unable to query GCABB through the macOS accessibility API")
}

let expected: [(identifier: String, role: String, pressable: Bool)] = [
    ("sidebar", "AXGroup", false),
    ("sidebar-toggle", "AXButton", true),
    ("destination-home", "AXButton", true),
    ("new-session", "AXButton", true),
    ("chats-home", "AXButton", true),
    ("composer-input", "AXTextField", true),
    ("home-submit-prompt", "AXButton", true),
    ("mode", "AXPopUpButton", true),
    ("model", "AXPopUpButton", true),
    ("effort", "AXPopUpButton", true),
]

for item in expected {
    guard let element = waitForElement(item.identifier, in: app) else {
        fail("missing accessibility element: \(item.identifier)")
    }
    let actualRole = text(element, kAXRoleAttribute as CFString) ?? ""
    guard actualRole == item.role else {
        fail(
            "\(item.identifier) has role \(actualRole), expected \(item.role)"
        )
    }
    guard !item.pressable || actions(element).contains(kAXPressAction) else {
        fail("\(item.identifier) does not expose AXPress")
    }
    guard !(text(element, kAXTitleAttribute as CFString) ?? "").isEmpty else {
        fail("\(item.identifier) does not expose an accessible name")
    }
    if ["mode", "model", "effort"].contains(item.identifier) {
        guard !(text(element, kAXValueAttribute as CFString) ?? "").isEmpty else {
            fail("\(item.identifier) does not expose its current value")
        }
    }
}

guard let composer = find("composer-input", in: app) else {
    fail("composer-input disappeared from the accessibility tree")
}
guard actions(composer).contains(kAXPressAction) else {
    fail("composer-input does not expose AXPress")
}

let frontmostBefore = boolean(app, kAXFrontmostAttribute as CFString)
let composerPress = AXUIElementPerformAction(
    composer,
    kAXPressAction as CFString
)
guard composerPress == .success else {
    fail("AXPress failed for composer-input: \(composerPress.rawValue)")
}

var composerFocused = false
for _ in 0..<30 {
    let currentComposer = find("composer-input", in: app) ?? composer
    composerFocused = boolean(
        currentComposer,
        kAXFocusedAttribute as CFString
    )
    if composerFocused {
        break
    }
    Thread.sleep(forTimeInterval: 0.1)
}
guard composerFocused else {
    fail("AXPress did not focus composer-input")
}

guard let mode = find("mode", in: app) else {
    fail("mode control disappeared from the accessibility tree")
}
let modeBefore = text(mode, kAXValueAttribute as CFString) ?? ""
guard !modeBefore.isEmpty else {
    fail("mode control does not expose its current value")
}
let modePress = AXUIElementPerformAction(mode, kAXPressAction as CFString)
guard modePress == .success else {
    fail("AXPress failed for mode: \(modePress.rawValue)")
}

guard let menu = waitForElement("composer-control-menu", in: app) else {
    fail("pressing mode did not expose its option list")
}
guard text(menu, kAXRoleAttribute as CFString) == "AXList" else {
    fail("mode options are not exposed as a list")
}

var nextMode: AXUIElement?
for child in children(menu) {
    let identifier = text(child, kAXIdentifierAttribute as CFString) ?? ""
    let selected = boolean(child, kAXSelectedAttribute as CFString)
    if identifier.hasPrefix("mode-option-") && !selected {
        nextMode = child
        break
    }
}
guard let nextMode else {
    fail("mode option list does not expose an unselected option")
}
guard text(nextMode, kAXRoleAttribute as CFString) == "AXStaticText" else {
    fail("mode option has an unexpected accessibility role")
}
guard actions(nextMode).contains(kAXPressAction) else {
    fail("mode option does not expose AXPress")
}
let nextModePress = AXUIElementPerformAction(nextMode, kAXPressAction as CFString)
guard nextModePress == .success else {
    fail("AXPress failed for mode option: \(nextModePress.rawValue)")
}

var modeAfter = modeBefore
for _ in 0..<30 {
    if let currentMode = find("mode", in: app) {
        modeAfter = text(currentMode, kAXValueAttribute as CFString) ?? ""
    }
    if modeAfter != modeBefore {
        break
    }
    Thread.sleep(forTimeInterval: 0.1)
}
guard modeAfter != modeBefore else {
    fail("AXPress did not change the mode control")
}

print("macOS accessibility smoke check passed")
print("frontmost before composer AXPress: \(frontmostBefore)")
print("composer-input focused: \(composerFocused)")
print("mode changed: \(modeBefore) -> \(modeAfter)")
