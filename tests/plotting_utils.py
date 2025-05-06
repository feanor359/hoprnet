# tests/plotting_utils.py
import matplotlib.pyplot as plt
import numpy as np
import os
from datetime import datetime
import logging

def plot_transfer_time_histogram(transfer_times: list[float], config_details: dict, output_dir: str = "logs/plots"):
    """
    Generates and saves a histogram of packet transfer times.

    Args:
        transfer_times: A list of transfer times in seconds.
        config_details: A dictionary containing configuration details for the plot title.
                        Expected keys: 'local1_delay', 'local2_delay', 'local1_delay_range', 'local2_delay_range'.
        output_dir: The directory where the plot image will be saved.
    """
    # Create output directory if it doesn't exist
    os.makedirs(output_dir, exist_ok=True)

    # Generate timestamp for unique filenames
    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S") # Added seconds for more uniqueness

    # Calculate statistics
    if not transfer_times:
        logging.warning("No transfer times provided for plotting.")
        return None, None # Return None if no data

    avg_transfer_time = np.mean(transfer_times)
    std_transfer_time = np.std(transfer_times)

    # Create histogram
    plt.figure(figsize=(10, 6))
    n, bins, patches = plt.hist(
        transfer_times,
        bins=10,
        edgecolor='black',
        alpha=0.7
    )

    # Add labels and title
    plt.xlabel('Transfer Time (seconds)')
    plt.ylabel('Number of Packets')
    local1_delay = config_details.get('local1_delay', 'N/A')
    local2_delay = config_details.get('local2_delay', 'N/A')
    local1_delay_range = config_details.get('local1_delay_range', 'N/A')
    local2_delay_range = config_details.get('local2_delay_range', 'N/A')
    title = (f'Packet Transfer Time Distribution\n'
             f'Local1 Delay: {local1_delay}ms (Range: {local1_delay_range}ms), '
             f'Local2 Delay: {local2_delay}ms (Range: {local2_delay_range}ms)')
    plt.title(title)

    # Add mean and std dev lines
    plt.axvline(avg_transfer_time, color='r', linestyle='dashed', linewidth=2, label=f'Mean: {avg_transfer_time:.3f}s')
    plt.axvline(avg_transfer_time + std_transfer_time, color='g', linestyle='dashed', linewidth=2, label=f'Std Dev: {std_transfer_time:.3f}s')
    plt.axvline(avg_transfer_time - std_transfer_time, color='g', linestyle='dashed', linewidth=2)

    plt.legend()

    # Save plot
    plot_filename = os.path.join(output_dir, f"transfer_times_{timestamp}_l1_{local1_delay}r{local1_delay_range}_l2_{local2_delay}r{local2_delay_range}.png")
    try:
        plt.savefig(plot_filename)
        logging.info(f"Plot saved to: {plot_filename}")
    except Exception as e:
        logging.error(f"Failed to save plot: {e}")
        plot_filename = None # Indicate failure
    finally:
        plt.close() # Close the plot figure to free memory

    return plot_filename, {"mean": avg_transfer_time, "std_dev": std_transfer_time}
