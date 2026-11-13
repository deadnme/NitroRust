// Runs Acer's NitroSense.exe in-process and makes its main window a resizable,
// plain rectangle: the UI scales with the window instead of clipping, and the
// angled frame's transparent edges are cropped and filled.
// Usage: NitroSenseResizable.exe [width]   e.g. NitroSenseResizable.exe 1080
using System;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;

static class Launcher
{
    // Transparent margin around the frame bitmap (img_bg_nitro.png), in DIPs.
    static readonly Thickness FrameMargin = new Thickness(40, 33, 40, 35);
    // Edge colour of the frame bitmap; fills the chamfered corners and any letterbox space.
    static readonly Color Fill = Color.FromRgb(0x1B, 0x1B, 0x1B);

    static double startWidth;

    [STAThread]
    static int Main(string[] args)
    {
        string dir = AppDomain.CurrentDomain.BaseDirectory;
        string exe = Path.Combine(dir, "NitroSense.exe");
        // A fresh AppDomain running NitroSense.exe makes it the entry assembly,
        // so its pack:// resources and config resolve exactly as when run normally.
        var setup = new AppDomainSetup { ApplicationBase = dir, ConfigurationFile = exe + ".config" };
        AppDomain domain = AppDomain.CreateDomain("NitroSense", null, setup);
        domain.SetData("startWidth", args.Length > 0 ? args[0] : "");
        domain.DoCallBack(InstallHook);
        return domain.ExecuteAssembly(exe);
    }

    static void InstallHook()
    {
        double.TryParse(AppDomain.CurrentDomain.GetData("startWidth") as string, out startWidth);
        EventManager.RegisterClassHandler(typeof(Window), FrameworkElement.LoadedEvent,
            new RoutedEventHandler(OnWindowLoaded));
    }

    static void OnWindowLoaded(object sender, RoutedEventArgs e)
    {
        var w = (Window)sender;
        // Only the main window (skips the small loading splash and dialogs).
        if (Application.Current == null || w != Application.Current.MainWindow || w.ActualWidth < 1000) return;
        var content = w.Content as UIElement;
        if (content == null) return;

        // NitroSense's main window already has a Viewbox root; wrap only if a future version drops it.
        if (!(content is Viewbox))
        {
            w.Content = null;
            w.Content = new Viewbox { Child = content, Stretch = Stretch.Uniform };
        }

        // Crop the frame's transparent margin: negative margins shrink the size the
        // Viewbox scales, so the opaque frame reaches the window edges.
        var grid = w.FindName("Main_Grid") as FrameworkElement;
        double cropW = 0, cropH = 0;
        if (grid != null)
        {
            grid.Margin = new Thickness(-FrameMargin.Left, -FrameMargin.Top, -FrameMargin.Right, -FrameMargin.Bottom);
            cropW = FrameMargin.Left + FrameMargin.Right;
            cropH = FrameMargin.Top + FrameMargin.Bottom;
        }
        w.Background = new SolidColorBrush(Fill);

        double ratio = (w.ActualHeight - cropH) / (w.ActualWidth - cropW);
        w.SizeToContent = SizeToContent.Manual;
        w.MaxWidth = w.MaxHeight = double.PositiveInfinity;
        w.MinWidth = 400; w.MinHeight = 400 * ratio;
        w.ResizeMode = ResizeMode.CanResizeWithGrip;
        w.Width = startWidth > 0 ? startWidth : w.ActualWidth - cropW;
        w.Height = w.Width * ratio;
    }
}
