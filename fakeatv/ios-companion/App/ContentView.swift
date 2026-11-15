import SwiftUI

struct ContentView: View {
    @StateObject private var sync = NowPlayingSyncManager.shared
    @State private var showingSettings = false
    @State private var inputHost = ""

    var body: some View {
        NavigationStack {
            ZStack {
                Color(uiColor: .systemGroupedBackground)
                    .ignoresSafeArea()

                VStack(spacing: 24) {
                    // Header / Status
                    HStack {
                        HStack(spacing: 6) {
                            Circle()
                                .fill(sync.isConnected ? Color.green : Color.red)
                                .frame(width: 8, height: 8)
                            Text(sync.isConnected ? "Подключено" : "Поиск TV...")
                                .font(.caption.weight(.medium))
                                .foregroundColor(sync.isConnected ? .green : .red)
                        }
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                        .background(.ultraThinMaterial, in: Capsule())

                        Spacer()

                        Button {
                            inputHost = sync.tvHost
                            showingSettings = true
                        } label: {
                            Image(systemName: "slider.horizontal.3")
                                .font(.subheadline)
                                .foregroundColor(.primary)
                                .padding(8)
                                .background(.ultraThinMaterial, in: Circle())
                        }
                    }
                    .padding(.horizontal)

                    Spacer(minLength: 10)

                    // Square Artwork
                    Group {
                        if let art = sync.artworkImage {
                            Image(uiImage: art)
                                .resizable()
                                .aspectRatio(1.0, contentMode: .fit)
                        } else {
                            ZStack {
                                RoundedRectangle(cornerRadius: 24, style: .continuous)
                                    .fill(Color(uiColor: .secondarySystemGroupedBackground))
                                Image(systemName: "tv.fill")
                                    .font(.system(size: 64))
                                    .foregroundColor(.secondary)
                            }
                            .aspectRatio(1.0, contentMode: .fit)
                        }
                    }
                    .frame(maxWidth: 320, maxHeight: 320)
                    .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
                    .shadow(color: .black.opacity(0.18), radius: 20, x: 0, y: 10)
                    .padding(.horizontal, 24)

                    // Metadata
                    VStack(spacing: 6) {
                        Text(sync.media?.title ?? "Ничего не воспроизводится")
                            .font(.title3.weight(.bold))
                            .multilineTextAlignment(.center)
                            .lineLimit(2)
                            .foregroundColor(.primary)

                        Text(sync.media?.artist.isEmpty == false ? (sync.media?.artist ?? "") : "Google Chromecast")
                            .font(.subheadline.weight(.medium))
                            .foregroundColor(.secondary)
                    }
                    .padding(.horizontal, 24)

                    // Progress Bar
                    if let m = sync.media, m.duration > 0 {
                        VStack(spacing: 4) {
                            ProgressView(value: min(max(m.elapsed, 0), m.duration), total: m.duration)
                                .tint(.primary)
                            HStack {
                                Text(formatTime(m.elapsed))
                                Spacer()
                                Text("-" + formatTime(max(0, m.duration - m.elapsed)))
                            }
                            .font(.caption2.monospacedDigit())
                            .foregroundColor(.secondary)
                        }
                        .padding(.horizontal, 32)
                    }

                    // Playback Controls
                    HStack(spacing: 28) {
                        Button {
                            sync.sendCommand("skip", param: -10)
                        } label: {
                            Image(systemName: "gobackward.10")
                                .font(.title2)
                        }

                        Button {
                            sync.sendCommand("prev")
                        } label: {
                            Image(systemName: "backward.fill")
                                .font(.title2)
                        }

                        Button {
                            sync.sendCommand("toggle")
                        } label: {
                            ZStack {
                                Circle()
                                    .fill(.primary)
                                    .frame(width: 64, height: 64)
                                Image(systemName: (sync.media?.isPlaying ?? false) ? "pause.fill" : "play.fill")
                                    .font(.title)
                                    .foregroundColor(Color(uiColor: .systemBackground))
                            }
                        }

                        Button {
                            sync.sendCommand("next")
                        } label: {
                            Image(systemName: "forward.fill")
                                .font(.title2)
                        }

                        Button {
                            sync.sendCommand("skip", param: 10)
                        } label: {
                            Image(systemName: "goforward.10")
                                .font(.title2)
                        }
                    }
                    .foregroundColor(.primary)
                    .padding(.vertical, 8)

                    Spacer(minLength: 10)

                    // Quick launch Apple TV Remote
                    Button {
                        if let url = URL(string: "tvremote://") {
                            UIApplication.shared.open(url)
                        }
                    } label: {
                        HStack(spacing: 8) {
                            Image(systemName: "appletvremote.gen4.fill")
                                .font(.headline)
                            Text("Открыть Apple TV Remote")
                                .font(.subheadline.weight(.semibold))
                        }
                        .foregroundColor(.primary)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 14)
                        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                    }
                    .padding(.horizontal, 24)
                    .padding(.bottom, 12)
                }
            }
            .navigationTitle("Chromecast Media")
            .navigationBarTitleDisplayMode(.inline)
            .sheet(isPresented: $showingSettings) {
                NavigationStack {
                    Form {
                        Section("Адрес Android TV / Chromecast") {
                            TextField("IP адрес (например 192.168.33.128)", text: $inputHost)
                                .keyboardType(.numbersAndPunctuation)
                                .autocorrectionDisabled()
                        }

                        Section {
                            Button("Сохранить и подключиться") {
                                sync.updateHost(inputHost)
                                showingSettings = false
                            }
                        }
                    }
                    .navigationTitle("Настройки TV")
                    .navigationBarTitleDisplayMode(.inline)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) {
                            Button("Закрыть") {
                                showingSettings = false
                            }
                        }
                    }
                }
                .presentationDetents([.fraction(0.4)])
            }
        }
    }

    private func formatTime(_ seconds: Double) -> String {
        guard !seconds.isNaN, !seconds.isInfinite, seconds >= 0 else { return "0:00" }
        let s = Int(seconds)
        let m = s / 60
        let sec = s % 60
        return String(format: "%d:%02d", m, sec)
    }
}
