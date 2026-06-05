import path from "path"
import os from "os"

const app = "ainix"
const data = path.join(os.homedir(), ".local", "share", app)

export namespace Global {
  export const Path = {
    data,
    bin: path.join(data, "bin"),
    log: path.join(data, "log"),
  }
}
