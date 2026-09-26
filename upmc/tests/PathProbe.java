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
            Path version = destination.resolve("versions/fabric-loader-0.19.5-1.21.11");
            Files.createDirectories(version);
            Files.write(version.resolve("fabric-loader-0.19.5-1.21.11.json"),
                "{\"id\":\"fabric-loader-0.19.5-1.21.11\",\"mainClass\":\"fixture.Main\",\"libraries\":[]}".getBytes(StandardCharsets.UTF_8));
            Path library = destination.resolve("libraries/fixture/loader.jar");
            Files.createDirectories(library.getParent());
            Files.write(library, "runtime fixture".getBytes(StandardCharsets.UTF_8));
        }
        Files.write(destination.resolve(marker), "UPMC_JAR_PATH_OK".getBytes(StandardCharsets.UTF_8));
        Files.write(destination.resolve("installer-args.txt"), String.join("\n", args).getBytes(StandardCharsets.UTF_8));
        System.out.println("UPMC_JAR_PATH_OK");
    }
}
