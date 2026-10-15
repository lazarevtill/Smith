import Darwin
import Foundation
import WorkerCore

@main struct Entry {
  static func main() async {
    signal(SIGPIPE, SIG_IGN)
    let output = dup(STDOUT_FILENO)
    guard output >= 0, dup2(STDERR_FILENO, STDOUT_FILENO) >= 0 else { _exit(74) }
    let sink = EventSink(fd: output)
    let arguments = CommandLine.arguments
    guard arguments.count == 3, arguments[1] == "--scratch-root" else {
      sink.error(
        id: nil,
        WorkerError(
          "invalid_arguments", "Usage: smith-mlx-worker --scratch-root /absolute/private/directory")
      )
      return
    }
    do {
      let fd = try LocalPackage.openDirectory(arguments[2])
      close(fd)
    } catch {
      sink.error(
        id: nil, WorkerError("unsafe_scratch", "A private existing scratch directory is required."))
      return
    }
    let worker = Worker(backend: NativeBackend(scratch: arguments[2]), sink: sink)
    await Task.detached {
      let reader = FrameReader()
      do {
        while let frame = try reader.next() {
          let request: Request
          do { request = try Request.decode(frame) } catch let error as WorkerError {
            sink.error(id: nil, error)
            break
          } catch {
            sink.error(id: nil, WorkerError.invalid)
            break
          }
          if !(await worker.accept(request)) { break }
        }
      } catch let error as WorkerError { sink.error(id: nil, error) } catch {
        sink.error(id: nil, WorkerError("ipc_io", "IPC input failed."))
      }
      await worker.eof()
    }.value
    close(output)
  }
}
