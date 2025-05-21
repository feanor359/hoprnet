import matplotlib.pyplot as plt
import numpy as np
import os
from datetime import datetime
import logging

def plot_transfer_time_histogram(transfer_times: list[float], config_details: dict, output_dir: str = "logs/plots"):
    """
    generate histogram of transfer times, with standard deviatio nd mean pointers
    this is used to test if the mixer works correctly
    """
    os.makedirs(output_dir, exist_ok=True)

    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    if not transfer_times:
        return None, None

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
    plt.xlabel('transfer time (seconds)')
    plt.ylabel('number of packets')
    local1_delay = config_details.get('local1_delay', 'None')
    local2_delay = config_details.get('local2_delay', 'None')
    local1_delay_range = config_details.get('local1_delay_range', 'None')
    local2_delay_range = config_details.get('local2_delay_range', 'None')
    title = (f'Packet Transfer Time Distribution\n'
             f'Local1 Delay: {local1_delay}ms (Range: {local1_delay_range}ms), '
             f'Local2 Delay: {local2_delay}ms (Range: {local2_delay_range}ms)')
    plt.title(title)

    # Add mean and std dev lines
    plt.axvline(avg_transfer_time, color='r', linestyle='dashed', linewidth=2, label=f'Mean: {avg_transfer_time:.3f}s')
    plt.axvline(avg_transfer_time + std_transfer_time, color='g', linestyle='dashed', linewidth=2, label=f'Std Dev: {std_transfer_time:.3f}s')
    plt.axvline(avg_transfer_time - std_transfer_time, color='g', linestyle='dashed', linewidth=2)

    plt.legend()

    # save the plotted graf for later view
    plot_filename = os.path.join(output_dir, f"transfer_times_{timestamp}_l1_{local1_delay}r{local1_delay_range}_l2_{local2_delay}r{local2_delay_range}.png")
    try:
        plt.savefig(plot_filename)
        logging.info(f"Plot saved to: {plot_filename}")
    except Exception as e:
        logging.error(f"Failed to save plot: {e}")
        plot_filename = None
        plt.close()

    return plot_filename, {"mean": avg_transfer_time, "std_dev": std_transfer_time}
