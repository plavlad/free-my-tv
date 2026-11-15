import Foundation
import AVFoundation

final class SilentAudioPlayer {
    static let shared = SilentAudioPlayer()
    private var player: AVAudioPlayer?

    private init() {}

    func start() {
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default, options: [.mixWithOthers, .allowAirPlay])
            try session.setActive(true)

            if player == nil {
                let silentData = createSilentWavData(durationSeconds: 1.0)
                player = try AVAudioPlayer(data: silentData)
                player?.numberOfLoops = -1
                player?.volume = 0.001
                player?.prepareToPlay()
            }
            player?.play()
        } catch {
            print("[SilentAudioPlayer] Failed to activate audio session: \(error)")
        }
    }

    func stop() {
        player?.stop()
    }

    private func createSilentWavData(durationSeconds: Double) -> Data {
        let sampleRate: Int32 = 44100
        let numChannels: Int16 = 1
        let bitsPerSample: Int16 = 16
        let numSamples = Int(Double(sampleRate) * durationSeconds)
        let dataSize = Int32(numSamples * Int(numChannels) * Int(bitsPerSample / 8))

        var data = Data()
        data.append("RIFF".data(using: .ascii)!)
        var chunkSize = 36 + dataSize
        data.append(Data(bytes: &chunkSize, count: 4))
        data.append("WAVE".data(using: .ascii)!)
        data.append("fmt ".data(using: .ascii)!)
        var subchunk1Size: Int32 = 16
        data.append(Data(bytes: &subchunk1Size, count: 4))
        var audioFormat: Int16 = 1 // PCM
        data.append(Data(bytes: &audioFormat, count: 2))
        var channels = numChannels
        data.append(Data(bytes: &channels, count: 2))
        var sRate = sampleRate
        data.append(Data(bytes: &sRate, count: 4))
        var byteRate = sampleRate * Int32(numChannels) * Int32(bitsPerSample / 8)
        data.append(Data(bytes: &byteRate, count: 4))
        var blockAlign = Int16(numChannels * (bitsPerSample / 8))
        data.append(Data(bytes: &blockAlign, count: 2))
        var bps = bitsPerSample
        data.append(Data(bytes: &bps, count: 2))
        data.append("data".data(using: .ascii)!)
        var dSize = dataSize
        data.append(Data(bytes: &dSize, count: 4))

        let silence = [UInt8](repeating: 0, count: Int(dataSize))
        data.append(contentsOf: silence)
        return data
    }
}
