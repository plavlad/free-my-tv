import Foundation

public struct NowPlayingResponse: Codable {
    public let status: String
    public let hasMedia: Bool
    public let media: MediaDetails?

    enum CodingKeys: String, CodingKey {
        case status
        case hasMedia = "has_media"
        case media
    }
}

public struct MediaDetails: Codable, Equatable, Sendable {
    public let title: String
    public let artist: String
    public let album: String
    public let appName: String
    public let appBundleId: String
    public let duration: Double
    public let elapsed: Double
    public let rate: Double
    public let isPlaying: Bool
    public let hasArtwork: Bool

    enum CodingKeys: String, CodingKey {
        case title, artist, album
        case appName = "app_name"
        case appBundleId = "app_bundle_id"
        case duration, elapsed, rate
        case isPlaying = "is_playing"
        case hasArtwork = "has_artwork"
    }

    public static let empty = MediaDetails(
        title: "Ожидание воспроизведения...",
        artist: "Google Chromecast",
        album: "",
        appName: "",
        appBundleId: "",
        duration: 0,
        elapsed: 0,
        rate: 0,
        isPlaying: false,
        hasArtwork: false
    )
}
