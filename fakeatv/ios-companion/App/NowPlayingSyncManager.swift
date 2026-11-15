import Foundation
import UIKit
import MediaPlayer
import Combine

@MainActor
final class NowPlayingSyncManager: ObservableObject {
    static let shared = NowPlayingSyncManager()

    @Published var media: MediaDetails? = nil
    @Published var artworkImage: UIImage? = nil
    @Published var isConnected: Bool = false
    @Published var tvHost: String = "192.168.33.128"
    @Published var lastError: String? = nil

    private var sseTask: Task<Void, Never>?
    private var pollTimer: Timer?
    private var commandsConfigured = false
    private var lastArtworkHash: String = ""

    private init() {
        setupRemoteCommands()
        startSync()
    }

    func updateHost(_ newHost: String) {
        let trimmed = newHost.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed != tvHost else { return }
        tvHost = trimmed
        startSync()
    }

    func startSync() {
        sseTask?.cancel()
        pollTimer?.invalidate()

        SilentAudioPlayer.shared.start()

        // 1. Initial REST fetch
        Task {
            await fetchNowPlaying()
        }

        // 2. Start SSE listener
        sseTask = Task { [weak self] in
            await self?.runSSELoop()
        }

        // 3. Fallback poll timer (every 4 seconds)
        pollTimer = Timer.scheduledTimer(withTimeInterval: 4.0, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                await self?.fetchNowPlaying()
            }
        }
    }

    func fetchNowPlaying() async {
        guard let url = URL(string: "http://\(tvHost):8765/nowplaying") else { return }
        do {
            var request = URLRequest(url: url)
            request.timeoutInterval = 3.0
            let (data, response) = try await URLSession.shared.data(for: request)
            if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                let resp = try JSONDecoder().decode(NowPlayingResponse.self, from: data)
                self.isConnected = true
                self.lastError = nil
                if resp.hasMedia, let m = resp.media {
                    self.updatePlayback(m)
                } else {
                    self.clearPlayback()
                }
            }
        } catch {
            if !isConnected {
                self.lastError = error.localizedDescription
            }
        }
    }

    private func runSSELoop() async {
        while !Task.isCancelled {
            guard let url = URL(string: "http://\(tvHost):8765/nowplaying/events") else {
                try? await Task.sleep(nanoseconds: 2_000_000_000)
                continue
            }

            var request = URLRequest(url: url)
            request.timeoutInterval = 60.0

            do {
                let (bytes, response) = try await URLSession.shared.bytes(for: request)
                if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                    self.isConnected = true
                    self.lastError = nil

                    for try await line in bytes.lines {
                        if Task.isCancelled { break }
                        if line.hasPrefix("data: ") {
                            let jsonString = String(line.dropFirst(6))
                            if let data = jsonString.data(using: .utf8) {
                                if let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
                                    let hasMedia = json["has_media"] as? Bool ?? false
                                    if hasMedia, let mediaDict = json["media"] as? [String: Any],
                                       let mediaData = try? JSONSerialization.data(withJSONObject: mediaDict),
                                       let m = try? JSONDecoder().decode(MediaDetails.self, from: mediaData) {
                                        self.updatePlayback(m)
                                    } else if !hasMedia {
                                        self.clearPlayback()
                                    }
                                }
                            }
                        }
                    }
                }
            } catch {
                if !Task.isCancelled {
                    try? await Task.sleep(nanoseconds: 3_000_000_000)
                }
            }
        }
    }

    private func updatePlayback(_ m: MediaDetails) {
        self.media = m
        self.isConnected = true

        let artHash = "\(m.title)-\(m.hasArtwork)"
        if m.hasArtwork && artHash != lastArtworkHash {
            lastArtworkHash = artHash
            Task {
                await fetchArtwork()
            }
        } else if !m.hasArtwork {
            self.artworkImage = nil
            self.updateSystemNowPlayingInfo()
        } else {
            self.updateSystemNowPlayingInfo()
        }
    }

    private func clearPlayback() {
        self.media = nil
        self.artworkImage = nil
        self.lastArtworkHash = ""
        MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
    }

    private func fetchArtwork() async {
        guard let url = URL(string: "http://\(tvHost):8765/nowplaying/artwork") else { return }
        do {
            var request = URLRequest(url: url)
            request.timeoutInterval = 4.0
            let (data, response) = try await URLSession.shared.data(for: request)
            if let http = response as? HTTPURLResponse, http.statusCode == 200, let img = UIImage(data: data) {
                self.artworkImage = img
                self.updateSystemNowPlayingInfo()
            }
        } catch {
            print("[NowPlayingSync] Failed to fetch artwork: \(error)")
        }
    }

    private func updateSystemNowPlayingInfo() {
        guard let m = media else {
            MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
            return
        }

        var info: [String: Any] = [
            MPMediaItemPropertyTitle: m.title,
            MPMediaItemPropertyArtist: m.artist.isEmpty ? "Google Chromecast" : m.artist,
            MPMediaItemPropertyAlbumTitle: "Google Chromecast",
            MPNowPlayingInfoPropertyMediaType: MPNowPlayingInfoMediaType.video.rawValue,
            MPMediaItemPropertyPlaybackDuration: m.duration,
            MPNowPlayingInfoPropertyElapsedPlaybackTime: m.elapsed,
            MPNowPlayingInfoPropertyPlaybackRate: m.isPlaying ? (m.rate > 0 ? m.rate : 1.0) : 0.0,
            MPNowPlayingInfoPropertyDefaultPlaybackRate: 1.0
        ]

        if let art = artworkImage {
            info[MPMediaItemPropertyArtwork] = MPMediaItemArtwork(boundsSize: art.size) { _ in
                art
            }
        }

        MPNowPlayingInfoCenter.default().nowPlayingInfo = info
    }

    private func setupRemoteCommands() {
        guard !commandsConfigured else { return }
        commandsConfigured = true

        let center = MPRemoteCommandCenter.shared()

        center.playCommand.isEnabled = true
        center.playCommand.addTarget { [weak self] _ in
            self?.sendCommand("play")
            return .success
        }

        center.pauseCommand.isEnabled = true
        center.pauseCommand.addTarget { [weak self] _ in
            self?.sendCommand("pause")
            return .success
        }

        center.togglePlayPauseCommand.isEnabled = true
        center.togglePlayPauseCommand.addTarget { [weak self] _ in
            self?.sendCommand("toggle")
            return .success
        }

        center.nextTrackCommand.isEnabled = true
        center.nextTrackCommand.addTarget { [weak self] _ in
            self?.sendCommand("next")
            return .success
        }

        center.previousTrackCommand.isEnabled = true
        center.previousTrackCommand.addTarget { [weak self] _ in
            self?.sendCommand("prev")
            return .success
        }

        center.skipForwardCommand.isEnabled = true
        center.skipForwardCommand.preferredIntervals = [10]
        center.skipForwardCommand.addTarget { [weak self] _ in
            self?.sendCommand("skip", param: 10)
            return .success
        }

        center.skipBackwardCommand.isEnabled = true
        center.skipBackwardCommand.preferredIntervals = [10]
        center.skipBackwardCommand.addTarget { [weak self] _ in
            self?.sendCommand("skip", param: -10)
            return .success
        }

        center.changePlaybackPositionCommand.isEnabled = true
        center.changePlaybackPositionCommand.addTarget { [weak self] event in
            guard let posEvent = event as? MPChangePlaybackPositionCommandEvent else { return .commandFailed }
            self?.sendCommand("seek", param: Int(posEvent.positionTime))
            return .success
        }
    }

    func sendCommand(_ command: String, param: Int? = nil) {
        guard let url = URL(string: "http://\(tvHost):8765/nowplaying/command") else { return }
        Task {
            var bodyDict: [String: Any] = ["command": command]
            if let p = param {
                bodyDict["param"] = p
            }
            guard let jsonData = try? JSONSerialization.data(withJSONObject: bodyDict) else { return }

            var request = URLRequest(url: url)
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = jsonData
            request.timeoutInterval = 3.0

            _ = try? await URLSession.shared.data(for: request)
            await fetchNowPlaying()
        }
    }
}
