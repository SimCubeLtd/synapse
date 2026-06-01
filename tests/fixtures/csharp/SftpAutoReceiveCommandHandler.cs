namespace Ippex.Sftp.Receiving;

/// <summary>
/// Wolverine command describing a request to auto-receive files over SFTP.
/// </summary>
public record SftpAutoReceiveCommand(string Host, int Port, string RemotePath);

/// <summary>
/// The persisted result of an auto-receive run.
/// </summary>
public struct SftpAutoReceiveResult
{
    public int FilesReceived;
    public long BytesReceived;
}

/// <summary>
/// Transport-level statuses an SFTP session can be in.
/// </summary>
public enum SftpSessionState
{
    Disconnected,
    Connecting,
    Connected,
    Faulted,
}

/// <summary>
/// Abstraction over the underlying SFTP client so handlers can be unit tested.
/// </summary>
public interface ISftpClient
{
    SftpSessionState State { get; }

    Task ConnectAsync(string host, int port);

    Task<int> DownloadAsync(string remotePath, string localPath);
}

/// <summary>
/// Handles <see cref="SftpAutoReceiveCommand"/> messages.
/// </summary>
public class SftpAutoReceiveCommandHandler
{
    private readonly ISftpClient _client;

    public SftpAutoReceiveCommandHandler(ISftpClient client)
    {
        _client = client;
    }

    public async Task<SftpAutoReceiveResult> HandleAsync(SftpAutoReceiveCommand command)
    {
        await _client.ConnectAsync(command.Host, command.Port);
        var count = await _client.DownloadAsync(command.RemotePath, "./inbox");
        return new SftpAutoReceiveResult { FilesReceived = count, BytesReceived = 0 };
    }
}
