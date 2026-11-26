import CoreAudio
import Foundation

// The CoreAudio devices of this guest, as JSON, for the acceptance to pick its
// test-owned endpoints; or the choice of the default input device. Device
// properties only: nothing here opens a stream, so no consent dialog can appear.
//   audio list
//   audio default-input <device UID>
//   audio wait <device UID> <present|absent> <seconds>

func address(_ selector: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMaster)
}

func number(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> UInt32? {
    var property = address(selector, scope)
    var value: UInt32 = 0
    var size = UInt32(MemoryLayout<UInt32>.size)
    guard AudioObjectGetPropertyData(object, &property, 0, nil, &size, &value) == noErr else { return nil }
    return value
}

func text(_ object: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
    var property = address(selector)
    var value: Unmanaged<CFString>?
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(object, &property, 0, nil, &size, &value) == noErr, let value else { return nil }
    return value.takeRetainedValue() as String
}

func streams(_ device: AudioObjectID, _ scope: AudioObjectPropertyScope) -> Int {
    var property = address(kAudioDevicePropertyStreams, scope)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(device, &property, 0, nil, &size) == noErr else { return 0 }
    return Int(size) / MemoryLayout<AudioStreamID>.size
}

func code(_ status: OSStatus) -> String {
    String(format: "%c%c%c%c", (status >> 24) & 255, (status >> 16) & 255, (status >> 8) & 255, status & 255)
}

func devices() -> [AudioObjectID] {
    var property = address(kAudioHardwarePropertyDevices)
    var size: UInt32 = 0
    var status = AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &property, 0, nil, &size)
    guard status == noErr else { fail("the size of the device list could not be read (OSStatus \(status) \(code(status)))") }
    var identifiers = [AudioObjectID](repeating: 0, count: max(Int(size) / MemoryLayout<AudioObjectID>.size, 1))
    size = UInt32(identifiers.count * MemoryLayout<AudioObjectID>.size)
    status = AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &property, 0, nil, &size, &identifiers)
    guard status == noErr else { fail("the device list could not be read (OSStatus \(status) \(code(status)))") }
    return Array(identifiers.prefix(Int(size) / MemoryLayout<AudioObjectID>.size))
}

func describe(_ device: AudioObjectID) -> [String: Any] {
    let transport = number(device, kAudioDevicePropertyTransportType).map { String(format: "%c%c%c%c", ($0 >> 24) & 255, ($0 >> 16) & 255, ($0 >> 8) & 255, $0 & 255) } ?? "?"
    return [
        "uid": text(device, kAudioDevicePropertyDeviceUID) ?? "",
        "name": text(device, kAudioObjectPropertyName) ?? "",
        "id": Int(device),
        "input_channels": streams(device, kAudioObjectPropertyScopeInput),
        "output_channels": streams(device, kAudioObjectPropertyScopeOutput),
        "transport": transport,
    ]
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

func emit(_ value: Any) {
    let data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    print(String(data: data, encoding: .utf8)!)
}

let arguments = CommandLine.arguments
switch arguments.count > 1 ? arguments[1] : "list" {
case "list":
    emit(devices().map(describe))
case "default-input":
    guard arguments.count == 3, let chosen = devices().first(where: { text($0, kAudioDevicePropertyDeviceUID) == arguments[2] }) else {
        fail("usage: audio default-input <device UID> of a present device")
    }
    var property = address(kAudioHardwarePropertyDefaultInputDevice)
    var value = chosen
    let status = AudioObjectSetPropertyData(AudioObjectID(kAudioObjectSystemObject), &property, 0, nil, UInt32(MemoryLayout<AudioObjectID>.size), &value)
    guard status == noErr else { fail("setting the default input device failed (OSStatus \(status) \(code(status)))") }
    emit(["default_input": describe(chosen)])
case "temporary-input", "temporary-output":
    let output = arguments[1] == "temporary-output"
    let scope = output ? kAudioObjectPropertyScopeOutput : kAudioObjectPropertyScopeInput
    guard arguments.count == 4, let source = devices().first(where: { text($0, kAudioDevicePropertyDeviceUID) == arguments[2] }),
          streams(source, scope) > 0 else { fail("\(arguments[1]) needs a source UID with that capability and a new UID") }
    let properties: [String: Any] = [
        kAudioAggregateDeviceUIDKey: arguments[3],
        kAudioAggregateDeviceNameKey: output ? "Voiceless acceptance output" : "Voiceless acceptance microphone",
        kAudioAggregateDeviceIsPrivateKey: false,
        kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: arguments[2]]],
    ]
    var device = AudioObjectID(kAudioObjectUnknown)
    let created = AudioHardwareCreateAggregateDevice(properties as CFDictionary, &device)
    guard created == noErr else { fail("creating aggregate device failed: \(created)") }
    // EOF disconnects just this aggregate. Its underlying device and the audio server stay up.
    _ = readLine()
    let destroyed = AudioHardwareDestroyAggregateDevice(device)
    guard destroyed == noErr else { fail("removing aggregate device failed: \(destroyed)") }
case "wait":
    guard arguments.count == 5, arguments[3] == "present" || arguments[3] == "absent", let limit = Double(arguments[4]) else {
        fail("usage: audio wait <device UID> <present|absent> <seconds>")
    }
    let wanted = arguments[3] == "present"
    let deadline = Date(timeIntervalSinceNow: limit)
    var found = false
    while true {
        found = devices().contains { text($0, kAudioDevicePropertyDeviceUID) == arguments[2] }
        if found == wanted || Date() >= deadline { break }
        Thread.sleep(forTimeInterval: 0.25)
    }
    emit(["uid": arguments[2], "wanted": arguments[3], "found": found])
    if found != wanted { exit(2) }
default:
    fail("usage: audio list | default-input <UID> | wait <UID> <present|absent> <seconds>")
}
