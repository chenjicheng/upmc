import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

/** No network or game installation: only record where the caller asked us to write. */
public final class PathProbe {
    public static void main(String[] args) throws Exception {
        Path destination = Paths.get(".");
        String marker = "packwiz-path-probe.txt";
        if (args.length > 0 && args[0].equals("client")) {
            marker = "fabric-path-probe.txt";
            for (int i = 0; i + 1 < args.length; i++) {
                if (args[i].equals("-dir")) destination = Paths.get(args[i + 1]);
            }
        }
        Files.write(destination.resolve(marker), "UPMC_JAR_PATH_OK".getBytes(StandardCharsets.UTF_8));
        System.out.println("UPMC_JAR_PATH_OK");
    }
}
